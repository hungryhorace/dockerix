// Builds, tests and lints the host-side `dockerix` CLI (cli/) on both a mac
// and an amd64 agent. Each agent needs a Rust toolchain on PATH (rustup with
// the clippy and rustfmt components); the amd64 agent also needs Docker,
// for the image and the linters (pinned images, pulled on first use), and
// the ROMs and disc image in ~/.dockerix/assets for the smoke test.
//
// .github/workflows/ci.yml runs the matrix's Rust stages (Toolchain to
// Doc on both platforms, Deny and Lint on Linux) on GitHub for pushes and
// pull requests, with the same pinned tool versions. Keep the stage list
// and the pins in step when changing either; Image, Smoke and Install are
// deliberately here only.
pipeline {
    agent none

    options {
        timestamps()
        disableConcurrentBuilds()
        buildDiscarder(logRotator(numToKeepStr: '20'))
    }

    environment {
        CARGO_TERM_COLOR = 'always'
        CARGO_INCREMENTAL = '0'
    }

    stages {
        stage('CI') {
            matrix {
                axes {
                    axis {
                        name 'PLATFORM'
                        values 'mac', 'amd64'
                    }
                }
                agent { label "${PLATFORM}" }
                stages {
                    stage('Toolchain') {
                        steps {
                            sh 'rustc --version && cargo --version && cargo clippy --version && cargo fmt --version'
                        }
                    }
                    stage('Build') {
                        steps {
                            dir('cli') {
                                sh 'cargo build --locked --all-targets'
                            }
                        }
                    }
                    stage('Test') {
                        steps {
                            dir('cli') {
                                sh 'cargo test --locked'
                            }
                        }
                    }
                    // Formatting is settled by cli/rustfmt.toml; any diff
                    // from it fails the build.
                    stage('Fmt') {
                        steps {
                            dir('cli') {
                                sh 'cargo fmt --check'
                            }
                        }
                    }
                    stage('Clippy') {
                        steps {
                            dir('cli') {
                                sh 'cargo clippy --locked --all-targets -- -D warnings'
                            }
                        }
                    }
                    // The doc comments, private items included (the default
                    // for a binary): broken links and bad markup fail.
                    stage('Doc') {
                        environment { RUSTDOCFLAGS = '-D warnings' }
                        steps {
                            dir('cli') {
                                sh 'cargo doc --locked --no-deps'
                            }
                        }
                    }
                    // Dependencies against cli/deny.toml: advisories,
                    // licences, duplicate versions, sources. The graph is the
                    // same on both platforms, so it is checked once.
                    // cargo-deny is pinned and installed into the workspace
                    // on first use (a few minutes, once per agent).
                    stage('Deny') {
                        when { environment name: 'PLATFORM', value: 'amd64' }
                        steps {
                            dir('cli') {
                                sh '''
                                    deny="$WORKSPACE/.ci-tools/bin/cargo-deny"
                                    "$deny" --version 2>/dev/null | grep -qx 'cargo-deny 0.20.2' \
                                        || cargo install --locked --root "$WORKSPACE/.ci-tools" cargo-deny@0.20.2
                                    "$deny" --locked check
                                '''
                            }
                        }
                    }
                    // hadolint on the Dockerfile (.hadolint.yaml) and
                    // shellcheck on scripts/, from pinned images. The files go
                    // in on stdin rather than a bind mount, so it works
                    // whether or not this agent shares paths with the Docker
                    // daemon.
                    stage('Lint') {
                        when { environment name: 'PLATFORM', value: 'amd64' }
                        steps {
                            sh '''
                                tar -c Dockerfile .hadolint.yaml \
                                    | docker run --rm -i --entrypoint sh hadolint/hadolint:v2.15.1-debian \
                                        -c 'cd "$(mktemp -d)" && tar -x && hadolint Dockerfile'
                                tar -c scripts \
                                    | docker run --rm -i --entrypoint sh koalaman/shellcheck-alpine:v0.11.0 \
                                        -c 'cd "$(mktemp -d)" && tar -x && shellcheck scripts/*.sh'
                            '''
                        }
                    }
                    // The container image, on the agent that has Docker, built
                    // by the CLI from its embedded sources exactly as a first
                    // `dockerix start` would. It needs no ROMs or disc image:
                    // those are mounted at run time. Then its binary is run,
                    // so the stage proves it executes, not only that the
                    // layers assemble.
                    stage('Image') {
                        when { environment name: 'PLATFORM', value: 'amd64' }
                        steps {
                            // `cargo run`, as the agents set CARGO_TARGET_DIR.
                            // `build` prints the image's tag on stdout.
                            dir('cli') {
                                sh '''
                                    tag=$(cargo run --locked -q -- build)
                                    docker run --rm --entrypoint dockerix "$tag" --version
                                '''
                            }
                        }
                    }
                    // End to end against the real guest (scripts/smoke.sh):
                    // boot on that image, push, compile and run hello.c, an
                    // exit status, `get`; then `restart --nfs` and the live
                    // mount (an edit reaching `make`, `put`, `status -v`, a
                    // guest `rm`), halt. Two boots, about 3 minutes. The ROMs
                    // and disc image are the agent's own, in
                    // ~/.dockerix/assets (never committed); without them it
                    // passes with a message. The agent has one executor, so
                    // nothing else holds dockerix-vm meanwhile.
                    stage('Smoke') {
                        when { environment name: 'PLATFORM', value: 'amd64' }
                        options { timeout(time: 20, unit: 'MINUTES') }
                        steps {
                            sh 'scripts/smoke.sh'
                        }
                    }
                }
            }
        }
        // Once everything above is green on both platforms, install this
        // `dockerix` on the amd64 agent, where another project's CI uses it; `Image`
        // has already built its image on the same Docker. From main only.
        stage('Install') {
            when {
                beforeAgent true
                branch 'main'
            }
            agent { label 'amd64' }
            steps {
                sh 'just install'
                sh 'dockerix --version'
            }
        }
    }
}
