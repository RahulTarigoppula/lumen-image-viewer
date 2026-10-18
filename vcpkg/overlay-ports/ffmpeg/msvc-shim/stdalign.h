/* Lumen build shim: older Windows SDKs (10.0.19041) ship no C11 <stdalign.h>.
 * Only used when the compiler's own header is missing (this directory is
 * searched after the system include paths via -I order in the port). */
#ifndef LUMEN_STDALIGN_SHIM_H
#define LUMEN_STDALIGN_SHIM_H
#if defined(_MSC_VER) && !defined(__clang__)
#ifndef alignas
#define alignas(x) __declspec(align(x))
#endif
#ifndef alignof
#define alignof(x) __alignof(x)
#endif
#define __alignas_is_defined 1
#define __alignof_is_defined 1
#endif
#endif
