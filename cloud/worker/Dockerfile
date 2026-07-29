# ATC Cloud worker image — Cloud ATC vertical slice ([[tasks/harmony-844]]).
#
# Boots on an ephemeral Fly Machine. Contains the tools the entrypoint needs:
# git, claude (headless), the NATS CLI, jq/curl/openssl (GitHub App token mint
# + self-destruct), and bash. No secrets and no PAT are baked in — the GitHub
# App key, NATS creds, and FLY_API_TOKEN are provided as Fly app secrets.
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Use bash with pipefail so a failing command in a `curl … | bash` pipe aborts
# the build instead of being masked by the pipe's last-command exit status.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl jq git openssl bash xz-utils \
    && rm -rf /var/lib/apt/lists/*

# NATS CLI (stream-json tee target). Pin a version for reproducibility.
ARG NATS_CLI_VERSION=0.1.6
RUN curl -fsSL "https://github.com/nats-io/natscli/releases/download/v${NATS_CLI_VERSION}/nats-${NATS_CLI_VERSION}-linux-amd64.zip" -o /tmp/nats.zip \
    && (cd /tmp && (unzip nats.zip || (apt-get update && apt-get install -y unzip && unzip nats.zip))) \
    && install -m 0755 "/tmp/nats-${NATS_CLI_VERSION}-linux-amd64/nats" /usr/local/bin/nats \
    && rm -rf /tmp/nats*

# Claude Code headless CLI. Installed from the official channel; the worker runs
# `claude --output-format stream-json` exactly like a local ATC dispatch. PATH is
# set before the install so the post-install check resolves the freshly-installed
# binary. The build fails fast if the CLI is missing (entrypoint.sh needs it at
# runtime), rather than masking the failure with `|| true`.
ENV PATH="/root/.local/bin:${PATH}"
RUN curl -fsSL https://claude.ai/install.sh | bash \
    && command -v claude >/dev/null

COPY entrypoint.sh gh-app-token.sh self-destruct.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/entrypoint.sh \
             /usr/local/bin/gh-app-token.sh \
             /usr/local/bin/self-destruct.sh

# The forked warm bare-mirror volume mounts here (fly machine run --volume).
VOLUME ["/workspace"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
