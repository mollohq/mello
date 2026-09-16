cmake_minimum_required(VERSION 3.20)

# The game capture hook is a separate project from libmello on purpose:
#  - It builds for x86 as well as x64. libmello is x64 only.
#  - It loads into other people's processes, so it links the static CRT and
#    depends on nothing that the game does not already have.
#
# Build it with hook/build.ps1, which configures both architectures.
project(mello_hook VERSION 0.1.0 LANGUAGES CXX)

if(NOT WIN32)
    message(FATAL_ERROR "[mello-hook] Windows only")
endif()

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POLICY_DEFAULT_CMP0091 NEW)
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

if(CMAKE_SIZEOF_VOID_P EQUAL 8)
    set(MELLO_HOOK_BITS 64)
else()
    set(MELLO_HOOK_BITS 32)
endif()
message(STATUS "[mello-hook] building the ${MELLO_HOOK_BITS}-bit set")

find_path(DETOURS_INCLUDE_DIRS "detours/detours.h" REQUIRED)
find_library(DETOURS_LIBRARY detours REQUIRED)

# --- The hook itself ---------------------------------------------------------
# Loads into the game. Every source in here runs in someone else's process.
add_library(mello_hook SHARED
    src/hook_main.cpp
    src/hook_state.cpp
    src/hook_dxgi.cpp
    src/hook_log.cpp
)
set_target_properties(mello_hook PROPERTIES OUTPUT_NAME "mello-hook${MELLO_HOOK_BITS}")
target_include_directories(mello_hook PRIVATE include ${DETOURS_INCLUDE_DIRS})
target_compile_definitions(mello_hook PRIVATE
    WIN32_LEAN_AND_MEAN NOMINMAX MELLO_HOOK_BITS=${MELLO_HOOK_BITS})
target_link_libraries(mello_hook PRIVATE ${DETOURS_LIBRARY} d3d11 dxgi dxguid)
# Exports the hook procedure under its plain name on x86 as well as x64.
set_target_properties(mello_hook PROPERTIES
    LINK_FLAGS "/DEF:\"${CMAKE_CURRENT_SOURCE_DIR}/src/mello_hook.def\"")

# --- Offsets helper ----------------------------------------------------------
# Builds a throwaway device outside the game and prints the present-function
# offsets. The hook must never do this inside a game process.
add_executable(mello_offsets src/offsets_main.cpp src/hook_log.cpp)
set_target_properties(mello_offsets PROPERTIES OUTPUT_NAME "mello-offsets${MELLO_HOOK_BITS}")
target_include_directories(mello_offsets PRIVATE include)
target_compile_definitions(mello_offsets PRIVATE
    WIN32_LEAN_AND_MEAN NOMINMAX MELLO_HOOK_BITS=${MELLO_HOOK_BITS})
target_link_libraries(mello_offsets PRIVATE d3d11 dxgi dxguid version)

# --- Injection helper --------------------------------------------------------
# Loads the hook into the game with SetWindowsHookEx. Its bitness must match
# the game's.
add_executable(mello_inject src/inject_main.cpp src/hook_log.cpp)
set_target_properties(mello_inject PROPERTIES OUTPUT_NAME "mello-inject${MELLO_HOOK_BITS}")
target_include_directories(mello_inject PRIVATE include)
target_compile_definitions(mello_inject PRIVATE
    WIN32_LEAN_AND_MEAN NOMINMAX MELLO_HOOK_BITS=${MELLO_HOOK_BITS})

# --- Test game ---------------------------------------------------------------
# A D3D11 program that presents, so the hook can be tested without a game
# installed. It is part of the test set, not of a release.
option(MELLO_HOOK_TEST_APP "Build the D3D11 test program the hook tests use" ON)
if(MELLO_HOOK_TEST_APP)
    add_executable(mello_fakegame src/fakegame_main.cpp)
    set_target_properties(mello_fakegame PROPERTIES OUTPUT_NAME "mello-fakegame${MELLO_HOOK_BITS}")
    target_include_directories(mello_fakegame PRIVATE include)
    target_compile_definitions(mello_fakegame PRIVATE
        WIN32_LEAN_AND_MEAN NOMINMAX MELLO_HOOK_BITS=${MELLO_HOOK_BITS})
    target_link_libraries(mello_fakegame PRIVATE d3d11 dxgi dxguid)
endif()

# --- Tests -------------------------------------------------------------------
# The protocol has one owner per field and a fixed layout. Both are worth a
# test, and neither needs a game.
option(MELLO_HOOK_TESTS "Build the hook tests" ON)
if(MELLO_HOOK_TESTS)
    enable_testing()
    find_package(GTest CONFIG QUIET)
    if(GTest_FOUND)
        add_executable(mello_hook_tests tests/test_hook_protocol.cpp src/hook_state.cpp src/hook_log.cpp)
        target_include_directories(mello_hook_tests PRIVATE include src ${DETOURS_INCLUDE_DIRS})
        target_compile_definitions(mello_hook_tests PRIVATE
            WIN32_LEAN_AND_MEAN NOMINMAX MELLO_HOOK_BITS=${MELLO_HOOK_BITS})
        target_link_libraries(mello_hook_tests PRIVATE GTest::gtest GTest::gtest_main d3d11 dxgi dxguid)
        add_test(NAME mello_hook_tests COMMAND mello_hook_tests)
    else()
        message(STATUS "[mello-hook] gtest not found, tests skipped")
    endif()
endif()
