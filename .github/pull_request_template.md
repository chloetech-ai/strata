## What and why

## One-way doors

Does this change the on-disk format (including stored layouts such as rollups, analyzers' names and archives), the shard protocol (`chloedb-server/proto`), replication, authentication, or what people install (the install scripts, the images)? If so, tick "Yes", say which, and what happens to existing databases, replicas and clients. A change to one of them waits for a human review. The paths of each door are in `.github/CODEOWNERS`, and the check `One-way doors` fails when this box and the diff disagree.

- [ ] No one-way door
- [ ] Yes:

## Tests

What was run, and on which machine for any number quoted.
