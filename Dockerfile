# Build a small image for Cloud Run.
FROM rust:1.97.1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/cd-service /cd-service
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/cd-service"]
