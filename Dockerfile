# The image is the product's claim made literal: a static binary, the
# dashboard's build output, and nothing else. FROM scratch is not a flex —
# with two dependencies and no libc requirement there is nothing to put in a
# base layer.
#
#   docker build -t traza .
#   TOKEN="rw:$(openssl rand -hex 16)"
#   docker run -p 8080:8080 -v traza-data:/data -e TRAZA_TOKENS="$TOKEN" traza
#
# The non-loopback bind refuses to start without TRAZA_TOKENS (or an explicit
# --allow-unauthenticated-non-loopback), in the container as anywhere else.
# The server runs as uid 65534; /data ships owned by that uid so a named
# volume inherits it. A bind mount must be writable by 65534.

FROM node:22-alpine AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:1-alpine AS server
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked --bin traza-server
RUN mkdir /data && chown 65534:65534 /data

FROM scratch
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/toshish/traza" \
      org.opencontainers.image.description="Trace datastore with first-class LLM and agent observability — one binary" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=server /src/target/release/traza-server /traza-server
COPY --from=ui /src/ui/dist /ui/dist
COPY LICENSE NOTICE THIRD_PARTY_NOTICES.md /
COPY --from=server --chown=65534:65534 /data /data
USER 65534:65534
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/traza-server", "--data-dir", "/data", "--host", "0.0.0.0", "--ui-dir", "/ui/dist"]
