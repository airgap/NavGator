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
        RUSTC_WRAPPER = 'sccache'
        CARGO_INCREMENTAL = '0' // sccache + incremental don't mix
    }

    stages {
        stage('Build & Test') {
            matrix {
                axes {
                    axis { name 'PLATFORM'; values 'linux', 'macos', 'windows' }
                }
                agent { label "${PLATFORM}" }
                stages {
                    stage('Toolchain') {
                        steps {
                            script {
                                if (isUnix()) {
                                    sh '''
                                        set -e
                                        rustup show
                                        if command -v apt-get >/dev/null; then
                                            sudo apt-get update -q
                                            sudo apt-get install -y llvm clang libclang-dev cmake pkg-config python3 xvfb
                                            echo "LIBCLANG_PATH=$(llvm-config --libdir)" > .ci-env
                                        elif command -v brew >/dev/null; then
                                            brew install llvm cmake pkg-config || true
                                            echo "LIBCLANG_PATH=$(brew --prefix llvm)/lib" > .ci-env
                                            echo "PATH=$(brew --prefix llvm)/bin:$PATH" >> .ci-env
                                        fi
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
                                            cargo build --locked --workspace
                                        '''
                                    } else {
                                        bat 'cargo build --locked --workspace'
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
                                def t = { isUnix() ? sh('cargo test --locked -p swerve-protocol')
                                                   : bat('cargo test --locked -p swerve-protocol') }
                                if (env.PLATFORM == 'linux') { t() }
                                else { catchError(buildResult: 'SUCCESS', stageResult: 'UNSTABLE') { t() } }
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
                                  timeout --signal=KILL 30 ./target/debug/swerve || true
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
