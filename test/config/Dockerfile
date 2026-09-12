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

# One-shot: replace the volume's contents with this build's files, readable by
# the container user (UID 1000), then exit.
ENTRYPOINT ["sh", "-ec", "rm -rf /config/* && cp -r /files/. /config/ && chown -R 1000:1000 /config && chmod -R a+rX /config && ls /config"]
