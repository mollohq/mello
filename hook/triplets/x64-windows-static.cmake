# Matches vcpkg's own x64-windows-static. Kept next to the x86 triplet so both
# architectures read their settings from the same place.
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
