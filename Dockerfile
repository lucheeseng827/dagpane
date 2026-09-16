# dagpane — a static musl binary on `scratch`.
#
# The image is the binary and the app you give it. No shell, no package manager, no libc,
# no Python and no Node — which is most of the point: the front end is compiled into the
# executable as one HTML file, so there is no build step to reproduce inside the image and
# nothing for it to fetch at run time.
#
#   docker build -t dagpane .
#   docker run --rm -p 8787:8787 -v "$PWD/examples:/app" dagpane run /app/sales.toml --host 0.0.0.0
#
# NOTE THE `--host 0.0.0.0`, and note what it prints. There is **no authentication in this
# version**: `dagpane run` binds loopback by default and warns when told to bind anything
# else, and inside a container "anything else" is the only useful choice. Put it behind
# something that authenticates before it is reachable by anyone you would not show the data
# to. See SECURITY.md.

FROM rust:1.94-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

# LICENSE and NOTICE are copied into the builder because the final stage copies them OUT of
# it. Staging only the sources built a binary and then failed on `COPY --from=build
# /src/LICENSE`, and nothing in CI built the image, so it would have shipped broken to the
# mirror — where the Dockerfile is the documented deployment path. `.github/workflows/ci.yml` now builds it.
COPY Cargo.toml Cargo.lock LICENSE NOTICE ./
COPY crates ./crates

# --locked: the lockfile is the build. It matters more here than in most projects, because
# this module's claims are counts and a count is only meaningful against a fixed dependency
# set — see the note in .gitignore.
RUN cargo build --release --locked --target x86_64-unknown-linux-musl -p dagpane-cli \
 && strip target/x86_64-unknown-linux-musl/release/dagpane

# Prove the static claim inside the builder, where a failure is a build failure rather than
# a runtime surprise on somebody else's machine.
RUN ldd target/x86_64-unknown-linux-musl/release/dagpane 2>&1 \
      | grep -Eq "not a dynamic executable|statically linked"

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/dagpane /dagpane
COPY --from=build /src/LICENSE /LICENSE
COPY --from=build /src/NOTICE /NOTICE

EXPOSE 8787
ENTRYPOINT ["/dagpane"]
CMD ["--help"]
