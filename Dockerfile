# dagpane — a static musl binary on `scratch`.
#
# The image is the binary and the app you give it. No shell, no package manager, no libc,
# no Python and no Node — which is most of the point: the front end is compiled into the
# executable as one HTML file, so there is no build step to reproduce inside the image and
# nothing for it to fetch at run time.
#
#   docker build -t dagpane .
#   docker run --rm -p 8787:8787 -v "$PWD/examples:/app" \
#     dagpane serve /app/sales.toml --origin http://localhost:8787
#
# `serve` binds every interface already, which is what a container needs. The `--origin` is
# not decoration: the WebSocket origin allowlist is otherwise derived from the address bound,
# so a wildcard bind allows an origin no browser sends and the page loads with dead controls.
#
# Note what it prints. There is **no authentication unless you ask for it**: on a wildcard
# bind `serve` warns that anyone who can reach the port can read every pane. Pass
# `--auth-jwks` and it verifies a bearer token itself, or put something that authenticates
# in front, before it is reachable by anyone you would not show the data to. See SECURITY.md.
#
# `docker stop` stops it. The binary is PID 1 here, where SIGTERM has no default action, so
# it handles the signal itself: it fails readiness, drains for `--drain-seconds` (5 by
# default), and exits 0. .github/workflows/ci.yml runs this image that way on every push — a browser's
# upgrade, a stranger's refused, then a stop — and .github/workflows/docker.yml repeats it on the image
# it publishes.

FROM rust:1.94-alpine AS build
# binutils for readelf, which is what actually answers the static question below.
# `strip` happens to come from the same package.
RUN apk add --no-cache musl-dev binutils
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
#
# NOT `ldd`. This asked ldd for "not a dynamic executable" or "statically linked", which is
# glibc's wording, and the builder is Alpine. musl's ldd prints the loader path for a
# static binary and a dynamic one alike:
#
#     $ ldd ./static-binary
#             /lib/ld-musl-x86_64.so.1 (0x7d02d07f4000)
#     $ ldd ./dynamic-binary
#             /lib/ld-musl-x86_64.so.1 (0x79e406f88000)
#
# So the check was not merely worded for the wrong libc, it was reading an answer that
# carries no information here. It never ran until the first real image build, because the
# CI that builds this Dockerfile lives on the public mirror and the mirror had never been
# synced.
#
# A PT_INTERP program header is the thing that actually differs: a dynamically linked
# executable names its interpreter, a static one has nothing to name. The output is printed
# rather than swallowed by `grep -q`, so a future failure says what it saw.
RUN BIN=target/x86_64-unknown-linux-musl/release/dagpane \
 && readelf -l "$BIN" > /tmp/prog-headers.txt \
 && if grep -q INTERP /tmp/prog-headers.txt; then \
      echo "ERROR: the musl build is not static - it names an interpreter:"; \
      grep -A1 INTERP /tmp/prog-headers.txt; \
      exit 1; \
    fi \
 && echo "static: no PT_INTERP segment, nothing to load at run time"

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/dagpane /dagpane
COPY --from=build /src/LICENSE /LICENSE
COPY --from=build /src/NOTICE /NOTICE

EXPOSE 8787
ENTRYPOINT ["/dagpane"]
CMD ["--help"]
