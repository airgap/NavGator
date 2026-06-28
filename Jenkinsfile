// navgator CI — tri-platform build on Linux / macOS / Windows (ROADMAP §R2/D2).
//
// Runs on Jenkins agents labelled `linux`, `macos`, `windows`. Building navgator
// compiles our forked engine (mozjs/SpiderMonkey + ANGLE + servo + stylo + webrender)
// — a multi-GB, long build, so the agents should be provisioned like lyku's
// `lyku-ci-base`: a single consistent LLVM, Rust (via rust-toolchain.toml), sccache,
// and warm `~/.cargo` caches. Linux is the required gate; macOS/Windows run
// non-blocking (UNSTABLE, not FAILURE) until they are green, then flip to required.
//
// Modeled on /raid/lyku/Jenkinsfile (GenericTrigger webhook, options, post-cleanup).

pipeline {
    agent none

    options {
        timestamps()
        ansiColor('xterm')
        buildDiscarder(logRotator(numToKeepStr: '20'))
        timeout(time: 180, unit: 'MINUTES')
        // A new dev push aborts the prior still-running build rather than piling up a 2nd
        // concurrent build. The pile-up was the root of the "jamming": two builds spawned a
        // 2nd mac-mini workspace (navgator-ci@2), contended the single macOS executor, and
        // forced manual mid-build aborts — and an abort mid-git-write leaves the workspace
        // "appears to be corrupt", failing the next checkout. One build at a time, supersede
        // cleanly; the engine build is ~1-2h so racing several is pure waste anyway.
        disableConcurrentBuilds(abortPrevious: true)
        // Each matrix cell does its own self-healing checkout (see the Checkout stage), so a
        // corrupt agent workspace re-clones instead of failing — disable the implicit checkout.
        skipDefaultCheckout(true)
    }

    triggers {
        // Poll GitHub for new `dev` commits (a localhost Jenkins can't receive a
        // GitHub push webhook); GenericTrigger remains for when a webhook is wired.
        pollSCM('H/5 * * * *')
        // Nightly TimerTrigger now drives just the Upstream canary (gated below); macOS builds
        // on every push again, so it's no longer nightly-gated.
        cron('H 3 * * *')
        GenericTrigger(
            genericVariables: [
                [key: 'ref', value: '$.ref'],
                [key: 'pusher', value: '$.pusher.login']
            ],
            token: 'navgator-webhook',
            causeString: 'Triggered by push from $pusher',
            printContributedVariables: true,
            printPostContent: false
        )
    }

    environment {
        CARGO_TERM_COLOR = 'always'
        // sccache is opt-in: the Toolchain stage enables RUSTC_WRAPPER=sccache (via
        // .ci-env) only if sccache is installed on the agent, so an agent without it
        // still builds (just without the compile cache).
    }

    stages {
        stage('Build & Test') {
            matrix {
                axes {
                    // 'windows' temporarily disabled (windows-strix offline) — re-add it
                    // here when the runner is online; the windows build branches remain below.
                    axis { name 'PLATFORM'; values 'linux', 'macos' }
                }
                // macOS builds on every push again: sccache + warm ~/.cargo make the repeat
                // engine build fast, and the per-push stall that once justified nightly-only is
                // now handled by disableConcurrentBuilds(abortPrevious) + the self-healing
                // checkout below — not by skipping macOS. Linux stays the required gate; macOS is
                // non-blocking (UNSTABLE) until green.
                agent { label "${PLATFORM}" }
                stages {
                    stage('Checkout') {
                        // Explicit, self-healing checkout (the implicit one is skipped). A corrupt
                        // or locked agent workspace ('.git appears to be corrupt', seen on the
                        // mac-mini after abortPrevious kills a build mid-git-write). Linux is the
                        // gate (plain checkout); macOS stays non-blocking and time-bounded.
                        steps {
                            script {
                                if (env.PLATFORM == 'linux') {
                                    checkout scm
                                } else {
                                    // No sh step here: durable-task's sh control-dir setup hangs if the
                                    // agent's storage is wedged (seen: InterruptedException in
                                    // FilePath.mkdirs ~5 min in, killing the cell and the whole build).
                                    // Pure git-plugin + deleteDir steps avoid that. The git plugin
                                    // self-heals a corrupt .git by re-cloning; the timeouts bound its
                                    // 10-min `git rev-parse` probe so a bad workspace can't stall the
                                    // build, and catchError keeps a macOS checkout failure non-blocking
                                    // (UNSTABLE). Re-clone once into a nuked workspace on the first stall.
                                    catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                                        try {
                                            timeout(time: 8, unit: 'MINUTES') { checkout scm }
                                        } catch (err) {
                                            echo "macOS checkout failed/timed out (${err}); nuking workspace and re-cloning"
                                            deleteDir()
                                            timeout(time: 12, unit: 'MINUTES') { checkout scm }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    stage('Toolchain') {
                        steps {
                            script {
                                if (isUnix()) {
                                    sh '''
                                        set -e
                                        # Jenkins `sh` is a non-login shell: it does NOT source the profile that
                                        # puts ~/.cargo/bin (or Homebrew) on PATH, so an installed rustup/brew is
                                        # invisible. Put the toolchains on PATH ourselves; bootstrap rustup only
                                        # if it is truly absent.
                                        export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/lib/llvm-18/bin:$PATH"
                                        . "$HOME/.cargo/env" 2>/dev/null || true
                                        command -v rustup >/dev/null || \
                                          curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
                                        export PATH="$HOME/.cargo/bin:$PATH"
                                        rustup show   # installs the toolchain pinned by rust-toolchain.toml

                                        # LLVM for mozjs/bindgen (needs llvm-objdump + a matching libclang). Best-effort.
                                        LIBCLANG=""
                                        if command -v brew >/dev/null; then
                                            brew list llvm >/dev/null 2>&1 || brew install llvm cmake pkg-config || true
                                            brew list gstreamer >/dev/null 2>&1 || brew install gstreamer || true
                                            # Build tools to compile + STATIC-link dav1d from source (AVIF; no runtime dylib to
                                            # bundle/sign — LYK-1297/1298). See SYSTEM_DEPS_DAV1D_BUILD_INTERNAL in .ci-env.
                                            for t in meson ninja nasm; do brew list "$t" >/dev/null 2>&1 || brew install "$t" || true; done
                                            P="$(brew --prefix llvm 2>/dev/null)"
                                            [ -n "$P" ] && { export PATH="$P/bin:$PATH"; LIBCLANG="$P/lib"; }
                                        elif command -v apt-get >/dev/null; then
                                            sudo -n apt-get update -q && sudo -n apt-get install -y llvm clang libclang-dev cmake pkg-config python3 xvfb libunwind-dev meson ninja-build nasm libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav || true
                                            LIBCLANG="$(llvm-config --libdir 2>/dev/null || echo /usr/lib/llvm-18/lib)"
                                        fi
                                        # Best-effort appimagetool for the Package stage's AppImage build (Linux only).
                                        # If it can't be fetched, package.sh degrades to a clean "skipping AppImage".
                                        if command -v apt-get >/dev/null && ! command -v appimagetool >/dev/null; then
                                            mkdir -p "$HOME/.local/bin"
                                            if [ ! -x "$HOME/.local/bin/appimagetool" ]; then
                                                curl -fsSL -o "$HOME/.local/bin/appimagetool" \
                                                  https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage \
                                                  && chmod +x "$HOME/.local/bin/appimagetool" || true
                                            fi
                                            export PATH="$HOME/.local/bin:$PATH"
                                        fi
                                        # Persist PATH (+ LIBCLANG_PATH) for the Build/Test/smoke stages (same workspace).
                                        # SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always → build dav1d from source and static-link it
                                        # (no runtime libdav1d to bundle/sign; AVIF decode, LYK-1297/1298). Needs meson/ninja/nasm.
                                        { echo "PATH=$PATH"; [ -n "$LIBCLANG" ] && echo "LIBCLANG_PATH=$LIBCLANG"; echo "SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always"; command -v sccache >/dev/null && echo "RUSTC_WRAPPER=sccache"; } > .ci-env
                                        rustc --version && cargo --version
                                    '''
                                } else {
                                    // Windows agent: LLVM + Rust assumed pre-provisioned on the runner.
                                    bat 'rustup show && where clang && where llvm-objdump'
                                }
                            }
                        }
                    }

                    stage('Build') {
                        steps {
                            script {
                                def build = {
                                    if (isUnix()) {
                                        sh '''
                                            set -e
                                            [ -f .ci-env ] && set -a && . ./.ci-env && set +a
                                            # Ensure dav1d is (re)built static-from-source: setting
                                            # SYSTEM_DEPS_DAV1D_BUILD_INTERNAL does NOT invalidate a dav1d-sys
                                            # build cached dynamically before the var existed, so bust it once
                                            # per workspace (marker persists). AVIF; LYK-1297/1298.
                                            [ -f .dav1d-static-done ] || cargo clean -p dav1d-sys 2>/dev/null || true
                                            cargo build --release --locked --workspace
                                            touch .dav1d-static-done
                                        '''
                                    } else {
                                        bat 'cargo build --release --locked --workspace'
                                    }
                                }
                                // Linux is the gate; macOS/Windows are non-blocking until green.
                                if (env.PLATFORM == 'linux') {
                                    build()
                                } else {
                                    catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') { build() }
                                }
                            }
                        }
                    }

                    stage('Test') {
                        steps {
                            script {
                                // Pure unit tests (servo-free); cross-platform.
                                def t = {
                                    if (isUnix()) {
                                        sh 'set -a; [ -f .ci-env ] && . ./.ci-env; set +a; cargo test --locked -p navgator-protocol'
                                    } else {
                                        bat 'cargo test --locked -p navgator-protocol'
                                    }
                                }
                                if (env.PLATFORM == 'linux') { t() }
                                else { catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') { t() } }
                            }
                        }
                    }

                    stage('Package') {
                        // Build the release artifacts (reusing the release build above) and
                        // stash them per platform. The post-matrix Publish stage uploads
                        // everything from the linux agent (the only one with the Doppler
                        // token). Wrapped UNSTABLE so a packaging hiccup isn't a gate failure.
                        steps {
                            catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                                sh '''
                                    set -a; [ -f .ci-env ] && . ./.ci-env; set +a
                                    bash scripts/package.sh
                                '''
                            }
                            stash name: "dist-${env.PLATFORM}", includes: 'dist/**', allowEmpty: true
                        }
                    }

                    stage('Headless smoke') {
                        when { expression { env.PLATFORM == 'linux' } }
                        steps {
                            sh '''
                                [ -f .ci-env ] && set -a && . ./.ci-env && set +a
                                xvfb-run -a -s "-screen 0 1280x800x24" \
                                  env LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
                                  timeout --signal=KILL 30 ./target/release/navgator || true
                            '''
                            // Content-sandbox confinement gate: applies the production Landlock+seccomp
                            // policy and asserts the negative-capability battery (unauthorized file read,
                            // TCP connect, inet-socket creation all DENIED; exit 0 iff caged, no panic).
                            // UNSTABLE-wrapped until every linux agent's Landlock (kernel >= 5.13) is
                            // confirmed — per this file's 'UNSTABLE until green, then required' convention;
                            // then drop the wrapper to make a broken/absent sandbox a hard FAILURE.
                            catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                                sh '''
                                    [ -f .ci-env ] && set -a && . ./.ci-env && set +a
                                    ./target/release/navgator --sandbox-selftest
                                '''
                            }
                        }
                    }
                }
            }
        }

        // Collect each platform's stashed artifacts on the linux agent (which has the
        // Doppler token) and publish them all to R2 + lyku.org/apps. macOS can't publish
        // from its own agent (no token there), so we mirror lyku's desktop job: the mac
        // builds/packages, the linux agent publishes.
        stage('Publish to lyku.org/apps') {
            agent { label 'linux' }
            steps {
                checkout scm
                script {
                    ['dist-linux', 'dist-macos'].each { s ->
                        try { unstash s } catch (err) { echo "no stash ${s} (platform may have failed)" }
                    }
                }
                catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                    sh '''
                        ls -lh dist/ 2>/dev/null || true
                        bash scripts/publish.sh
                    '''
                }
            }
        }

        // Maintained-fork drift watch: report how far the engine forks trail upstream.
        // Cron-scheduled (configure in the Jenkins job) or manual; non-blocking.
        stage('Upstream canary') {
            agent { label 'linux' }
            when { anyOf { triggeredBy 'TimerTrigger'; expression { params.CANARY == true } } }
            steps {
                checkout scm
                catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                    sh 'bash scripts/sync-forks.sh --check'
                }
            }
        }
    }

    post {
        success { echo 'navgator CI passed' }
        unstable { echo 'navgator CI unstable (macOS/Windows not yet green, or upstream drift)' }
        failure { echo 'navgator CI failed' }
    }
}
