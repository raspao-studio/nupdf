# OSS-Fuzz image for nupdf (projects/nupdf/Dockerfile upstream).
FROM gcr.io/oss-fuzz-base/base-builder-rust
RUN git clone --depth 1 https://github.com/raspao-studio/nupdf.git nupdf
WORKDIR nupdf
COPY build.sh $SRC/
