# Security policy

## Reporting a vulnerability

Please report a vulnerability privately, through GitHub: open
[a private security advisory](https://github.com/chloetech-ai/strata/security/advisories/new)
on this repository. Don't open a public issue or pull request for it.

Say what is affected (Strata the library, `strata-server`, the Python
package, or Engram), the commit you tested, and how to reproduce it. The
report and the fix are discussed in the advisory, which is published once a
fix has merged.

## What is supported

There are no releases yet: fixes go to `main` only.

## What counts as a vulnerability

- Anything that lets a client of `strata-server` do more than its
  credential allows: an API key acting beyond its scope (`read`, `write`,
  `admin`), reaching the calls between the servers of a cluster without the
  cluster's token, or getting that token.
- Reading or changing data without a credential on a server that has one.
- A crafted database file, write-ahead log, model file, import (SQLite,
  CSV, JSON) or request that corrupts memory, reads outside its buffers, or
  makes a server fetch a URL its media allowlist refuses.
- Engram exposing one project's memory to another, or its inspector
  answering without its access token.

A server started with no token and no key is open by design: it is for a
machine of one's own, and refuses to listen beyond loopback.
Denial of service by an authenticated client (an expensive query, a large
import) is a bug to report as an issue, not an advisory.
