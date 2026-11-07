# The image worker-host's jobs run in (jobs.toml), built by job-runner on base-rust,
# which brings Rust 1.99.1, Zig 0.13.0 and cargo-zigbuild 0.24.1.
ARG BASE=localhost/base-rust:1
FROM ${BASE}

# The release publishes with gh and checks its binaries with file; the beta test
# reads WorkerService-beta's address and token with the AWS CLI.
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl unzip file \
 && rm -rf /var/lib/apt/lists/* \
 && curl -sSfL -o /tmp/awscli.zip https://awscli.amazonaws.com/awscli-exe-linux-x86_64-2.39.1.zip \
 && echo "5b6c10e0a8ccdcf30c84bd643fea511f103d257a4787a7cef878691d63169804  /tmp/awscli.zip" | sha256sum -c - \
 && unzip -q /tmp/awscli.zip -d /tmp \
 && /tmp/aws/install \
 && rm -rf /tmp/aws /tmp/awscli.zip \
 && curl -sSfL -o /tmp/gh.tar.gz https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_linux_amd64.tar.gz \
 && echo "4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d  /tmp/gh.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/gh.tar.gz -C /tmp \
 && install -m 0755 /tmp/gh_2.103.3_linux_amd64/bin/gh /usr/local/bin/gh \
 && rm -rf /tmp/gh.tar.gz /tmp/gh_2.103.3_linux_amd64
USER app

# The four targets a release ships; base-rust has only the gnu ones.
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin
