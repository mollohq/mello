# The 32-bit hook links the static CRT, so it depends on nothing the game does
# not already have. vcpkg ships no x86-windows-static triplet, so this is ours.
set(VCPKG_TARGET_ARCHITECTURE x86)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
