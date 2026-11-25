# Container image for @bitefoundry/atlassian (stdio MCP server, CLI, and TUI).
#
# Installs the verified Linux release binary (GLIBC <= 2.28) directly from
# GitHub Releases instead of compiling Rust from source, so builds complete in
# seconds and match the published release artifact.
#
# Examples:
#   docker run -i --rm \
#     -e ATLASSIAN_SITE_URL=https://acme.atlassian.net \
#     -e ATLASSIAN_EMAIL=you@acme.com \
#     -e ATLASSIAN_API_TOKEN=... \
#     atlassian
#
#   docker run -i --rm atlassian mcp --read-only
FROM debian:bookworm-slim

ARG ATLASSIAN_VERSION=0.8.4
ARG TARGETARCH

LABEL org.opencontainers.image.title="atlassian" \
      org.opencontainers.image.description="Single Rust binary pairing a stdio MCP server, CLI, local HTTP API, and Ratatui TUI for Jira and Confluence over an embedded SQLite WAL + FTS5 cache." \
      org.opencontainers.image.source="https://github.com/BiteFoundry/atlassian" \
      org.opencontainers.image.documentation="https://github.com/BiteFoundry/atlassian#readme" \
      org.opencontainers.image.version="${ATLASSIAN_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.BiteFoundry/atlassian"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) target="x86_64-unknown-linux-gnu" ;; \
      arm64) target="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/BiteFoundry/atlassian/releases/download/v${ATLASSIAN_VERSION}"; \
    archive="atlassian-${ATLASSIAN_VERSION}-${target}.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/SHA256SUMS.txt"; \
    grep -F "  ${archive}" SHA256SUMS.txt > expected.sha256; \
    test "$(wc -l < expected.sha256)" -eq 1; \
    sha256sum -c expected.sha256; \
    tar -xzf "${archive}" atlassian; \
    install -m 0755 atlassian /usr/local/bin/atlassian; \
    ln -s /usr/local/bin/atlassian /usr/local/bin/confluence; \
    ln -s /usr/local/bin/atlassian /usr/local/bin/conf; \
    rm -f "/tmp/${archive}" /tmp/SHA256SUMS.txt /tmp/expected.sha256 /tmp/atlassian; \
    atlassian --version

ENV ATLASSIAN_CONFIG_DIR=/data/config \
    ATLASSIAN_DATA_DIR=/data/state

RUN mkdir -p /data/config /data/state \
 && chmod 0777 /data /data/config /data/state

WORKDIR /data

ENTRYPOINT ["atlassian"]
CMD ["mcp"]
