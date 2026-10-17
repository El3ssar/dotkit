# Disposable endpoint for the real SSH smoke test. The compiled kit binary is mounted read-only.
FROM ubuntu:24.04
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    bash python3 openssh-client openssh-server git curl ca-certificates tar unzip xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
