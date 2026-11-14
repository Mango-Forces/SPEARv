# Runtime qualification image, not a replacement for the production engine.
FROM debian@sha256:ec8534cda2cb30f29f44f23e135297af7a1496f2990fd8e4e84aa36b081a757a
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 zlib1g libbz2-1.0 libffi8 liblzma5 \
    libreadline8 libsqlite3-0 libexpat1 libstdc++6 libgomp1 \
 && rm -rf /var/lib/apt/lists/*
COPY python /opt/spearv/python
COPY preflight.py manifest.json embed-probe /app/
ENV LD_LIBRARY_PATH=/opt/spearv/python/lib \
    PYTHONNOUSERSITE=1 PYTHONDONTWRITEBYTECODE=1 \
    HOME=/nonexistent OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
    MKL_NUM_THREADS=1 NUMEXPR_NUM_THREADS=1
USER 10001:10001
WORKDIR /app
CMD ["/opt/spearv/python/bin/python3.14", "-I", "-B", "/app/preflight.py"]
