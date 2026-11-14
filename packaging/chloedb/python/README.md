# chloedb (placeholder)

This release, 0.0.3, holds the name `chloedb` on PyPI for [ChloeDB](https://github.com/chloetech-ai/chloedb), an embedded search database with SQL. It is not ChloeDB: it contains no code, and importing it raises an error that says how to install ChloeDB.

ChloeDB itself is on PyPI from 0.3.0, as wheels for CPython 3.10 or later on Linux x86_64 and aarch64 (glibc 2.28 or later) and macOS 11 or later, so there `pip install chloedb` installs it. pip falls back to this placeholder only where no wheel fits (Windows, Alpine, an older Python). There, build ChloeDB from source, which needs Rust and a C compiler (on Windows, inside WSL 2):

```sh
git clone https://github.com/chloetech-ai/chloedb
pip install --upgrade ./chloedb/chloedb-py
```
