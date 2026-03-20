/*
 * Minimal cuda.h stub — provides only the types needed by nvcuvid.h / cuviddec.h.
 * The actual CUDA runtime is loaded dynamically via LoadLibrary at runtime;
 * we never link against libcuda. This stub lets the NVDEC headers compile
 * without requiring a full CUDA SDK installation.
 */

#ifndef __cuda_cuda_h__
#define __cuda_cuda_h__

#include <stddef.h>

#define CUDA_VERSION 12000
#define CUDA_SUCCESS 0

#if defined(_WIN32)
#define CUDAAPI __stdcall
#else
#define CUDAAPI
#endif

typedef int CUresult;
typedef int CUdevice;
typedef void* CUcontext;
typedef void* CUstream;
typedef void* CUarray;

#if defined(_WIN64) || defined(__LP64__) || defined(__x86_64) || defined(AMD64) || defined(_M_AMD64)
typedef unsigned long long CUdeviceptr;
#else
typedef unsigned int CUdeviceptr;
#endif

#endif /* __cuda_cuda_h__ */
