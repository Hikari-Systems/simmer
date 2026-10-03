# Carries the test tiers' Simmer configurations (test/config/*.yaml) into a named
# volume, which the tier's override mounts into `app` at /config.
#
# Why an image rather than a bind mount: a bind mount of a host path is the
# obvious tool and is not portable — in sandboxed Docker setups it silently
# mounts an empty directory, and the tier then fails with a missing config rather
# than a clear error. A build context is sent to the daemon through the API, so it
# works wherever `docker build` works. The shipped runtime image never contains
# these files.
FROM debian:bookworm-slim

COPY test/config/ /files/

# Every tier config gets a `<name>.capture.yaml` twin: the original, verbatim,
# plus capture.block.yaml. Generated here rather than committed because two files
# drift — a timeout changed in one and not the other would silently give a
# captured run a different shape from its uncaptured twin — and because the
# capture belongs to no particular tier. `SIMMER_CAPTURE=on` points `app` at the
# twin; see tests/compose/stack.rs.
#
# D-126's export works the same way: `<name>.otel.yaml` appends
# telemetry.block.yaml, and `<name>.capture.otel.yaml` appends both, so
# `SIMMER_CAPTURE=on` and `SIMMER_OTEL=on` combine freely.
RUN set -e; \
    for f in /files/simmer.*.yaml; do \
        b="${f%.yaml}"; \
        { cat "$f"; echo; cat /files/capture.block.yaml; } > "$b.capture.yaml"; \
        { cat "$f"; echo; cat /files/telemetry.block.yaml; } > "$b.otel.yaml"; \
        { cat "$b.capture.yaml"; echo; cat /files/telemetry.block.yaml; } > "$b.capture.otel.yaml"; \
    done

# One-shot: replace the volume's contents with this build's files, readable by
# the container user (UID 1000), then exit.
ENTRYPOINT ["sh", "-ec", "rm -rf /config/* && cp -r /files/. /config/ && chown -R 1000:1000 /config && chmod -R a+rX /config && ls /config"]
