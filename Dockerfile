# Single static binary (templates, static assets and migrations are compiled in).
FROM rust:1.96-alpine AS build
RUN apk add --no-cache musl-dev gcc g++ make cmake perl
WORKDIR /src
COPY . .
RUN cargo build --release --locked
# Data dir owned by the runtime user (distroless `nonroot`, uid 65532), so a fresh volume mounted
# here is writable.
RUN mkdir /data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/rlinks /rlinks
COPY --from=build --chown=65532:65532 /data /data
ENV LD_BIND=0.0.0.0:9090 LD_DATA_DIR=/data
VOLUME /data
EXPOSE 9090
ENTRYPOINT ["/rlinks"]
CMD ["serve"]
