Test-only TLS material for `tests/server.rs`: a throwaway CA (`ca.pem`, its
key discarded) and a certificate for `localhost` and `127.0.0.1` it signed
(`cert.pem`, `key.pem`). Generated with openssl, valid for 100 years. Never
use them for anything but these tests.
