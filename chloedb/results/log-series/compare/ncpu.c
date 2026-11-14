// Makes a process see NCPU cores (default 4): interposes sysconf so that
// _SC_NPROCESSORS_ONLN / _SC_NPROCESSORS_CONF, which Rust's
// std::thread::available_parallelism reads on macOS, return NCPU.
// cc -dynamiclib -o ncpu.dylib ncpu.c
// DYLD_INSERT_LIBRARIES=ncpu.dylib NCPU=4 program ...
#include <stdlib.h>
#include <unistd.h>
static long my_sysconf(int name) {
  if (name == _SC_NPROCESSORS_ONLN || name == _SC_NPROCESSORS_CONF) {
    const char *n = getenv("NCPU");
    return n ? atol(n) : 4;
  }
  return sysconf(name);
}
__attribute__((used)) static struct { const void *r, *o; } interpose[]
  __attribute__((section("__DATA,__interpose"))) = {{(const void *)my_sysconf, (const void *)sysconf}};
