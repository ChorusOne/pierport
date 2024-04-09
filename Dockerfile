from rust:1.76-alpine3.19

RUN apk add musl-dev
RUN mkdir pierport
COPY Cargo.lock Cargo.toml pierport

# Prebuild the target dir, just for quicker testing when iterating
RUN cd pierport && \
        mkdir src && \
        touch src/lib.rs && \
        echo "fn main() {}" > src/main.rs && \
        cargo install --path . && \
        rm -rf src
COPY src/ pierport/src
# If we prebuild the target dir, we must touch the source files, because otherwise they are ignored
RUN touch pierport/src/*

RUN cd pierport && cargo install --path . && rm -rf target
COPY pierport_vere_db.json /
COPY scripts/env_cfg.sh scripts/entrypoint.sh /
RUN chmod +x env_cfg.sh entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
