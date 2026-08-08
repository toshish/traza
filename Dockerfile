# The image is the product's claim made literal: a static binary, the
# dashboard's build output, and nothing else. FROM scratch is not a flex —
# with two dependencies and no libc requirement there is nothing to put in a
# base layer.
#
#   docker build -t traza .
#   docker run -p 8080:8080 -v traza-data:/data \
#     -e TRAZA_TOKENS="rw:$(openssl rand -hex 16)" traza
#
# The non-loopback bind refuses to start without TRAZA_TOKENS (or an explicit
# --allow-unauthenticated-non-loopback), in the container as anywhere else.

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

FROM scratch
COPY --from=server /src/target/release/traza-server /traza-server
COPY --from=ui /src/ui/dist /ui/dist
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/traza-server", "--data-dir", "/data", "--host", "0.0.0.0", "--ui-dir", "/ui/dist"]
