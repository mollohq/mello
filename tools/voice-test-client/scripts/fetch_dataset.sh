#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA_DIR="${ROOT_DIR}/test-data"
TMP_DIR="${DATA_DIR}/tmp"
CLEAN_DIR="${DATA_DIR}/clean"
NOISE_DIR="${DATA_DIR}/noise"
PAIRS_DIR="${DATA_DIR}/pairs"

LIBRISPEECH_URL="${LIBRISPEECH_URL:-https://www.openslr.org/resources/12/dev-clean.tar.gz}"
MUSAN_URL="${MUSAN_URL:-https://www.openslr.org/resources/17/musan.tar.gz}"
NOIZEUS_INDEX_URL="${NOIZEUS_INDEX_URL:-https://ecs.utdallas.edu/loizou/speech/noizeus/}"
LIBRISPEECH_SAMPLE_COUNT="${LIBRISPEECH_SAMPLE_COUNT:-8}"
MUSAN_SAMPLE_COUNT="${MUSAN_SAMPLE_COUNT:-8}"
NOIZEUS_SAMPLE_COUNT="${NOIZEUS_SAMPLE_COUNT:-16}"
NOIZEUS_SAMPLES_PER_ZIP="${NOIZEUS_SAMPLES_PER_ZIP:-1}"

mkdir -p "${TMP_DIR}" "${CLEAN_DIR}" "${NOISE_DIR}" "${PAIRS_DIR}"

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required tool: $1" >&2
    exit 1
  fi
}

require_tool curl
require_tool ffmpeg
require_tool tar
require_tool unzip
require_tool rg

download_if_missing() {
  local url="$1"
  local out="$2"
  if [[ -f "${out}" ]]; then
    echo "Using cached $(basename "${out}")"
    return
  fi
  echo "Downloading ${url}"
  curl -fL "${url}" -o "${out}"
}

download_with_fallback_tls() {
  local url="$1"
  local out="$2"
  if [[ -f "${out}" ]]; then
    echo "Using cached $(basename "${out}")"
    return 0
  fi
  if curl -fL "${url}" -o "${out}"; then
    return 0
  fi
  # NOIZEUS host may fail TLS chain validation on some systems.
  curl -kfL "${url}" -o "${out}"
}

convert_to_contract() {
  local input="$1"
  local output="$2"
  ffmpeg -hide_banner -loglevel error -y -i "${input}" -ac 1 -ar 48000 -sample_fmt s16 "${output}"
}

count_matching_files() {
  local dir="$1"
  local pattern="$2"
  local count=0
  local f
  shopt -s nullglob
  for f in "${dir}"/${pattern}; do
    [[ -f "${f}" ]] && count=$((count + 1))
  done
  shopt -u nullglob
  echo "${count}"
}

extract_samples_from_tar() {
  local archive="$1"
  local regex="$2"
  local out_dir="$3"
  local prefix="$4"
  local count="$5"

  local i
  i="$(count_matching_files "${out_dir}" "${prefix}_*.wav")"
  [[ "${i}" -ge "${count}" ]] && return 0
  while IFS= read -r path; do
    [[ -z "${path}" ]] && continue
    tar -xzf "${archive}" -C "${TMP_DIR}" "${path}"
    local src="${TMP_DIR}/${path}"
    local dst="${out_dir}/${prefix}_${i}.wav"
    convert_to_contract "${src}" "${dst}"
    i=$((i + 1))
    [[ "${i}" -ge "${count}" ]] && break
  done < <(tar -tzf "${archive}" | rg "${regex}")
}

extract_samples_from_zip() {
  local archive="$1"
  local regex="$2"
  local out_dir="$3"
  local prefix="$4"
  local count="$5"

  local i
  i="$(count_matching_files "${out_dir}" "${prefix}_*.wav")"
  [[ "${i}" -ge "${count}" ]] && return 0
  while IFS= read -r path; do
    [[ -z "${path}" ]] && continue
    unzip -o -qq "${archive}" "${path}" -d "${TMP_DIR}"
    local src="${TMP_DIR}/${path}"
    local dst="${out_dir}/${prefix}_${i}.wav"
    convert_to_contract "${src}" "${dst}"
    i=$((i + 1))
    [[ "${i}" -ge "${count}" ]] && break
  done < <(unzip -Z1 "${archive}" | rg "${regex}")
}

discover_noizeus_zip_urls() {
  if [[ -n "${NOIZEUS_ZIP_URLS:-}" ]]; then
    # Optional override: space/newline-separated list of direct zip URLs.
    printf '%s\n' "${NOIZEUS_ZIP_URLS}" | tr ' ' '\n' | rg '\.zip$' || true
    return
  fi

  local html=""
  if ! html="$(curl -fL "${NOIZEUS_INDEX_URL}")"; then
    html="$(curl -kfL "${NOIZEUS_INDEX_URL}")"
  fi

  printf '%s' "${html}" \
    | rg -o 'href="[^"]+\.zip"' \
    | sed -E 's/^href="([^"]+)"$/\1/' \
    | awk '!seen[$0]++' \
    | while IFS= read -r href; do
        [[ -z "${href}" ]] && continue
        if [[ "${href}" =~ ^https?:// ]]; then
          printf '%s\n' "${href}"
        else
          printf '%s/%s\n' "${NOIZEUS_INDEX_URL%/}" "${href}"
        fi
      done
}

echo "==> Fetching LibriSpeech clean clips"
LIBRI_ARCHIVE="${TMP_DIR}/librispeech-dev-clean.tar.gz"
have_clean="$(count_matching_files "${CLEAN_DIR}" "librispeech_*.wav")"
if [[ "${have_clean}" -ge "${LIBRISPEECH_SAMPLE_COUNT}" ]]; then
  echo "Skipping LibriSpeech conversion (${have_clean} files already present)"
else
  download_if_missing "${LIBRISPEECH_URL}" "${LIBRI_ARCHIVE}"
  extract_samples_from_tar "${LIBRI_ARCHIVE}" "\\.flac$" "${CLEAN_DIR}" "librispeech" "${LIBRISPEECH_SAMPLE_COUNT}"
fi

echo "==> Fetching MUSAN noise clips"
MUSAN_ARCHIVE="${TMP_DIR}/musan.tar.gz"
have_noise="$(count_matching_files "${NOISE_DIR}" "musan_*.wav")"
if [[ "${have_noise}" -ge "${MUSAN_SAMPLE_COUNT}" ]]; then
  echo "Skipping MUSAN conversion (${have_noise} files already present)"
else
  download_if_missing "${MUSAN_URL}" "${MUSAN_ARCHIVE}"
  extract_samples_from_tar "${MUSAN_ARCHIVE}" "\\.(wav|flac)$" "${NOISE_DIR}" "musan" "${MUSAN_SAMPLE_COUNT}"
fi

echo "==> Fetching NOIZEUS pairs"
NOIZEUS_URLS=()
while IFS= read -r url; do
  [[ -n "${url}" ]] && NOIZEUS_URLS+=("${url}")
done < <(discover_noizeus_zip_urls)
if [[ "${#NOIZEUS_URLS[@]}" -eq 0 ]]; then
  echo "Warning: NOIZEUS zip URLs not found. Set NOIZEUS_ZIP_URLS and rerun." >&2
else
  noizeus_i="$(count_matching_files "${PAIRS_DIR}" "noizeus_*.wav")"
  if [[ "${noizeus_i}" -ge "${NOIZEUS_SAMPLE_COUNT}" ]]; then
    echo "Skipping NOIZEUS conversion (${noizeus_i} files already present)"
  else
    echo "NOIZEUS: resuming from ${noizeus_i}/${NOIZEUS_SAMPLE_COUNT}"
  fi
  for url in "${NOIZEUS_URLS[@]}"; do
    [[ "${noizeus_i}" -ge "${NOIZEUS_SAMPLE_COUNT}" ]] && break

    base="$(basename "${url}")"
    archive="${TMP_DIR}/noizeus_${base}"

    if ! download_with_fallback_tls "${url}" "${archive}"; then
      echo "Warning: failed to download NOIZEUS archive: ${url}" >&2
      continue
    fi

    per_zip=0
    while IFS= read -r path; do
      [[ -z "${path}" ]] && continue
      unzip -o -qq "${archive}" "${path}" -d "${TMP_DIR}"
      src="${TMP_DIR}/${path}"
      dst="${PAIRS_DIR}/noizeus_${noizeus_i}.wav"
      convert_to_contract "${src}" "${dst}"
      noizeus_i=$((noizeus_i + 1))
      per_zip=$((per_zip + 1))
      [[ "${noizeus_i}" -ge "${NOIZEUS_SAMPLE_COUNT}" ]] && break
      [[ "${per_zip}" -ge "${NOIZEUS_SAMPLES_PER_ZIP}" ]] && break
    done < <(unzip -Z1 "${archive}" | rg '\.wav$' || true)
  done

  if [[ "${noizeus_i}" -eq 0 ]]; then
    echo "Warning: NOIZEUS archives downloaded but no WAV files were extracted." >&2
  fi
fi

echo "Done."
echo "Clean samples: ${CLEAN_DIR}"
echo "Noise samples: ${NOISE_DIR}"
echo "Pairs: ${PAIRS_DIR}"
