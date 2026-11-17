# Linux × shell compatibility matrix. macOS cannot run in Docker;
# macOS coverage lives in .github/workflows/ci.yml (macos-latest runners).
FROM rust:1.89-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    zsh fish tmux pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV CARGO_HUSKY_DONT_INSTALL_HOOKS=1 CI=1
RUN cargo build --release -p termcmp && cargo test --workspace --release

FROM ubuntu:24.04 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash zsh fish tmux ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/termcmp /usr/local/bin/termcmp
COPY --from=build /src/scripts /opt/termcmp/scripts
COPY --from=build /src/shell /opt/termcmp/shell
COPY --from=build /src/tests/shell /opt/termcmp/tests/shell
WORKDIR /opt/termcmp
CMD ["bash", "scripts/docker-shell-matrix.sh"]