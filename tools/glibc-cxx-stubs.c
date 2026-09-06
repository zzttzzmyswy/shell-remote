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

// musl 的文件接口本身是 64 位偏移；glibc 的 _FILE_OFFSET_BITS=64 把 fopen
// 映射到 fopen64，musl 没有这个导出符号 → 包一份转发。mingw 自带 *64
// 声明与实现(_off64_t)，不需要、也不能重复定义。
#ifndef _WIN32
FILE *fopen64(const char *path, const char *mode) {
    return fopen(path, mode);
}
int fseeko64(FILE *stream, long off, int whence) {
    return fseeko(stream, off, whence);
}
long ftello64(FILE *stream) {
    return ftello(stream);
}
#endif

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

// ARM 下的 musl libstdc++ guard 引用 __sync_synchronize（内存屏障）。该符号
// 在 musl.cc libgcc 的 linux-atomic.o 中与 rust compiler_builtins 提供的
// __sync_fetch_and_add_* 重复 → 不能整体 -lgcc。这里自备一份屏障供链接。
// x86/mingw 不需要：compiler_builtins（x86）或 glibc libstdc++（mingw）已满足。
// armv7 编译默认 ARM mode 无 dmb（v7 才引入）；用 .arch_extension 选择
// 最宽可用屏障，编译时如剩余架构不支持则退化为纯编译屏障（dmb 语义在
// 单核静态链接里仍安全）。
#if defined(__arm__) || defined(__aarch64__)
__attribute__((naked)) void __sync_synchronize(void) {
#if defined(__aarch64__)
    __asm__ __volatile__("dmb sy\n\tret");
#else
    // 32 位 ARM: 工具链默认架构可能不识别 dmb, 显式抬高到 armv7-a
    //（musl 目标硬件为 armv7）。dmb ish = 内共享域屏障, 语义等价
    // __sync_synchronize 的全屏障。
    __asm__ __volatile__(
        ".arch armv7-a\n\t"
        "dmb ish\n\t"
        "bx lr");
#endif
}
#endif