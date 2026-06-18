// swerve CI — tri-platform build on Linux / macOS / Windows (ROADMAP §R2/D2).
//
// Runs on Jenkins agents labelled `linux`, `macos`, `windows`. Building swerve
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
    }

    triggers {
        // Poll GitHub for new `dev` commits (a localhost Jenkins can't receive a
        // GitHub push webhook); GenericTrigger remains for when a webhook is wired.
        pollSCM('H/5 * * * *')
        GenericTrigger(
            genericVariables: [
                [key: 'ref', value: '$.ref'],
                [key: 'pusher', value: '$.pusher.login']
            ],
            token: 'swerve-webhook',
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
                agent { label "${PLATFORM}" }
                stages {
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
                                            P="$(brew --prefix llvm 2>/dev/null)"
                                            [ -n "$P" ] && { export PATH="$P/bin:$PATH"; LIBCLANG="$P/lib"; }
                                        elif command -v apt-get >/dev/null; then
                                            sudo -n apt-get update -q && sudo -n apt-get install -y llvm clang libclang-dev cmake pkg-config python3 xvfb || true
                                            LIBCLANG="$(llvm-config --libdir 2>/dev/null || echo /usr/lib/llvm-18/lib)"
                                        fi
                                        # Persist PATH (+ LIBCLANG_PATH) for the Build/Test/smoke stages (same workspace).
                                        { echo "PATH=$PATH"; [ -n "$LIBCLANG" ] && echo "LIBCLANG_PATH=$LIBCLANG"; command -v sccache >/dev/null && echo "RUSTC_WRAPPER=sccache"; } > .ci-env
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
                                            cargo build --release --locked --workspace
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
                                        sh 'set -a; [ -f .ci-env ] && . ./.ci-env; set +a; cargo test --locked -p swerve-protocol'
                                    } else {
                                        bat 'cargo test --locked -p swerve-protocol'
                                    }
                                }
                                if (env.PLATFORM == 'linux') { t() }
                                else { catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') { t() } }
                            }
                        }
                    }

                    stage('Package & Publish') {
                        // Package the release build (reused from Build) into tarball +
                        // AppImage (linux) / dmg (macos), upload to R2, and register with
                        // lyku.org/apps. Wrapped so a packaging/publish hiccup is UNSTABLE,
                        // not a gate failure; publish.sh no-ops if R2/Doppler creds absent.
                        steps {
                            catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                                sh '''
                                    set -a; [ -f .ci-env ] && . ./.ci-env; set +a
                                    bash scripts/package.sh
                                    bash scripts/publish.sh
                                '''
                            }
                        }
                    }

                    stage('Headless smoke') {
                        when { expression { env.PLATFORM == 'linux' } }
                        steps {
                            sh '''
                                [ -f .ci-env ] && set -a && . ./.ci-env && set +a
                                xvfb-run -a -s "-screen 0 1280x800x24" \
                                  env LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
                                  timeout --signal=KILL 30 ./target/release/swerve || true
                            '''
                        }
                    }
                }
            }
        }

        // Maintained-fork drift watch: report how far the engine forks trail upstream.
        // Cron-scheduled (configure in the Jenkins job) or manual; non-blocking.
        stage('Upstream canary') {
            agent { label 'linux' }
            when { anyOf { triggeredBy 'TimerTrigger'; expression { params.CANARY == true } } }
            steps {
                catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') {
                    sh 'bash scripts/sync-forks.sh --check'
                }
            }
        }
    }

    post {
        success { echo 'swerve CI passed' }
        unstable { echo 'swerve CI unstable (macOS/Windows not yet green, or upstream drift)' }
        failure { echo 'swerve CI failed' }
    }
}
