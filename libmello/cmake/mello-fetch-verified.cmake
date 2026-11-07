# mello_fetch_verified(<out_file> <sha512> <url> [<url>...])
#
# Puts the file with the pinned SHA512 at <out_file>. Each URL is a source of
# the same file. The function tries the URLs in the given order.
#
# - An <out_file> that exists and has the pinned SHA512 stays. No download.
# - An <out_file> with a different SHA512 is removed and downloaded again.
# - Each download goes to a temporary file first. The function checks the
#   download status and the SHA512 of that file. A pass renames it to
#   <out_file>. A fail removes it and tries the next URL.
# - When all URLs fail, the configure stops with a FATAL_ERROR. The message
#   gives each URL and the reason it failed.
#
# file(DOWNLOAD) gets no EXPECTED_HASH: a hash mismatch in file(DOWNLOAD) is
# an error that stops the configure, so the next URL is never tried.
function(mello_fetch_verified out_file sha512)
    set(urls ${ARGN})
    if(NOT urls)
        message(FATAL_ERROR "[mello] mello_fetch_verified(${out_file}): no URL given")
    endif()
    string(TOLOWER "${sha512}" want)
    get_filename_component(name "${out_file}" NAME)

    if(EXISTS "${out_file}")
        file(SHA512 "${out_file}" have)
        if(have STREQUAL want)
            message(STATUS "[mello] ${name}: the local copy has the pinned SHA512")
            return()
        endif()
        message(STATUS "[mello] ${name}: the local copy has a bad SHA512 (${have}). Remove it and download it again.")
        file(REMOVE "${out_file}")
    endif()

    get_filename_component(dir "${out_file}" DIRECTORY)
    file(MAKE_DIRECTORY "${dir}")
    # A unique name per configure: two builds can use the same folder at the
    # same time (MELLO_DEPS_DIR).
    string(RANDOM LENGTH 8 suffix)
    set(tmp "${out_file}.part-${suffix}")

    set(errors "")
    foreach(url IN LISTS urls)
        message(STATUS "[mello] ${name}: download from ${url}")
        file(REMOVE "${tmp}")
        file(DOWNLOAD "${url}" "${tmp}" STATUS st TIMEOUT 600 INACTIVITY_TIMEOUT 60)
        list(GET st 0 code)
        if(code EQUAL 0)
            file(SHA512 "${tmp}" have)
            if(have STREQUAL want)
                file(RENAME "${tmp}" "${out_file}")
                message(STATUS "[mello] ${name}: SHA512 OK, from ${url}")
                return()
            endif()
            set(reason "SHA512 mismatch (got ${have})")
        else()
            list(GET st 1 msg)
            set(reason "download error ${code}: ${msg}")
        endif()
        message(STATUS "[mello] ${url} failed: ${reason}")
        string(APPEND errors "\n  ${url}\n    ${reason}")
        file(REMOVE "${tmp}")
    endforeach()

    message(FATAL_ERROR "[mello] ${name}: all sources failed:${errors}")
endfunction()
