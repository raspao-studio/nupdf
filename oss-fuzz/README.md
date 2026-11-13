# OSS-Fuzz

The files for nupdf's project in [OSS-Fuzz](https://github.com/google/oss-fuzz): `project.yaml`, `Dockerfile` and `build.sh` go in `projects/nupdf/` there. Applying needs the repository to be public (M10).

Before sending them, check the build with OSS-Fuzz's helper, from a checkout of oss-fuzz with these files copied in:

```bash
python3 infra/helper.py build_image nupdf
python3 infra/helper.py build_fuzzers --sanitizer address nupdf
python3 infra/helper.py check_build nupdf
```

The build compiles every target in `fuzz/` and seeds the ones that read documents with `corpus/smoke`, and the font target with the base fonts, as CI does.
