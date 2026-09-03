// glibc-only symbol stubs for musl static links (see BUILD.md).
//
// OpenH264 is C++; the static musl binaries must drag in a C++ stdlib, and the
// distro sysroots only ship a glibc-built libstdc++.a. That archive references
// glibc-only symbols (__*_chk fortify helpers, __libc_single_threaded,
// __isoc23_strtoul) which musl does not provide. This file maps each of them to
// its plain C counterpart. Compile with the *target's* gcc:
//   <triplet>-gcc -O2 -c tools/glibc-cxx-stubs.c -o stub.o
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

int __libc_single_threaded = 1;

unsigned long __isoc23_strtoul(const char *nptr, char **endptr, int base) {
    return strtoul(nptr, endptr, base);
}

int __vfprintf_chk(FILE *stream, int flag, const char *fmt, va_list ap) {
    (void)flag;
    return vfprintf(stream, fmt, ap);
}
int __vsnprintf_chk(char *s, size_t maxlen, int flag, size_t slen,
                    const char *fmt, va_list ap) {
    (void)flag;
    (void)slen;
    return vsnprintf(s, maxlen, fmt, ap);
}
int __snprintf_chk(char *s, size_t maxlen, int flag, size_t slen,
                   const char *fmt, ...) {
    (void)flag;
    (void)slen;
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(s, maxlen, fmt, ap);
    va_end(ap);
    return r;
}
int __sprintf_chk(char *s, int flag, size_t slen, const char *fmt, ...) {
    (void)flag;
    (void)slen;
    va_list ap;
    va_start(ap, fmt);
    int r = vsprintf(s, fmt, ap);
    va_end(ap);
    return r;
}
char *__strcpy_chk(char *d, const char *s, size_t slen) {
    (void)slen;
    return strcpy(d, s);
}
char *__strncpy_chk(char *d, const char *s, size_t n, size_t slen) {
    (void)slen;
    return strncpy(d, s, n);
}
char *__strcat_chk(char *d, const char *s, size_t slen) {
    (void)slen;
    return strcat(d, s);
}
char *__strncat_chk(char *d, const char *s, size_t n, size_t slen) {
    (void)slen;
    return strncat(d, s, n);
}
void *__memcpy_chk(void *d, const void *s, size_t n, size_t slen) {
    (void)slen;
    return memcpy(d, s, n);
}
void *__memmove_chk(void *d, const void *s, size_t n, size_t slen) {
    (void)slen;
    return memmove(d, s, n);
}
void *__memset_chk(void *d, int c, size_t n, size_t slen) {
    (void)slen;
    return memset(d, c, n);
}