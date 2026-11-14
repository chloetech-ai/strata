# chloedb (placeholder)

This version, 0.0.3, holds the name `chloedb` on npm for [ChloeDB](https://github.com/chloetech-ai/chloedb), an embedded search database with SQL. It is not ChloeDB: it contains no code, and importing it throws an error that says how to install ChloeDB.

Install ChloeDB from its GitHub release, where each `chloedb-js-v<version>` release has a package per platform (linux-x64-gnu, linux-arm64-gnu, darwin-arm64, darwin-x64), for example:

```sh
npm install https://github.com/chloetech-ai/chloedb/releases/download/chloedb-js-v0.3.3/chloedb-0.3.3-darwin-arm64.tgz
```

The list of platforms is in [the JavaScript guide](https://chloetech-ai.github.io/chloedb/site/docs/javascript.html). Once ChloeDB's releases are published here, `npm install chloedb` installs the real package (0.3.0 and later), with the addon for your platform.
