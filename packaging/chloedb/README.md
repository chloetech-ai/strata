# chloedb placeholders

Version 0.0.3 of `chloedb` on PyPI, npm and crates.io holds the name for
[ChloeDB](https://github.com/chloetech-ai/chloedb), an embedded search
database with SQL, until releases go out under it. The packages contain no
code: each says how to install ChloeDB instead. The real packages' versions
(0.3.0 and later) are above the placeholders', so once py-release.yml and
js-release.yml publish them, `pip install chloedb` and `npm install chloedb`
take them. PyPI has them from 0.3.0, so there the placeholder is what pip
picks only on a platform with no wheel, and it says how to build from
source. (0.0.1, the first placeholders, pointed at the project's old
name and address.)

`.github/workflows/reserve-names.yml` publishes them, run by hand. Each
registry needs its setup first; the workflow's header says what. Once the
real packages are on npm (js-release.yml, by hand with `-f npm=true`) and
crates.io (crate-release.yml, by hand with `-f publish=true`), it leaves
those two registries alone.
