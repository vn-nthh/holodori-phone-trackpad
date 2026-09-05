#!/usr/bin/env bash
#
# Builds the Linux experimental release bundle: native-host release binary,
# Tauri launcher release binary, docs, and a tar.gz archive with checksums.
#
# This is the Linux counterpart to packaging/build-experimental.ps1. It
# mirrors that script's bundle layout (launcher at the top, native host in a
# platform-named subdirectory, docs in Docs/) but drops the two things that
# are Windows-specific there:
#   - CARGO_HOME / RUSTFLAGS="-C target-feature=+crt-static": that pins a
#     specific developer's drive letter and statically links the Windows CRT
#     for a dependency-free portable .exe. Linux binaries link glibc
#     dynamically as a matter of course, so neither applies here.
#   - the touch-probe / touch-smoke diagnostic binaries: both are
#     Windows-only (they drive the Windows Touch API) and exit 1 immediately
#     on Linux, so shipping them in the Linux bundle would just be dead
#     weight.
#
# The Android APK build is included but optional: it only runs when an
# Android SDK is available (via --android-sdk or the ANDROID_HOME /
# ANDROID_SDK_ROOT environment variables), and skips cleanly with a message
# otherwise. The Android app itself needs no Linux-specific changes -- it is
# host-OS agnostic -- so building it from Linux CI is not fundamentally
# different from building it from Windows CI. It is a separate deliverable
# from the native-host/launcher Linux port, so its absence is not a build
# failure.
#
# One more deliberate omission: build-experimental.ps1 also copies the
# signed APK to a standalone release/<name>-android.apk next to the bundle
# archive, in addition to the copy it places inside the bundle's Android/
# directory. This script only produces the copy inside the bundle; it does
# not also write a standalone release/<name>-android.apk.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: packaging/build-linux.sh [options]

Builds the Linux experimental release bundle under release/<name>/ and
release/<name>-linux-x64.tar.gz.

Options:
  -n, --name NAME          Bundle name (default: Doritrack-v<VERSION>)
      --android-sdk PATH   Android SDK root, enables the optional APK build.
                            Defaults to $ANDROID_SDK_ROOT or $ANDROID_HOME if set.
      --java-home PATH     JAVA_HOME for the Android/Gradle build.
                            Defaults to $JAVA_HOME if set.
  -h, --help                Show this help and exit.

Without --android-sdk (and no ANDROID_HOME/ANDROID_SDK_ROOT in the
environment), the Android APK build is skipped and the bundle ships without
an Android/ directory.
EOF
}

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_ROOT="${PROJECT_ROOT}/release"
VERSION="$(tr -d '[:space:]' <"${PROJECT_ROOT}/VERSION")"

NAME="Doritrack-v${VERSION}"
ANDROID_SDK="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
JAVA_HOME_ARG="${JAVA_HOME:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
    -n | --name)
        if [[ $# -lt 2 ]]; then
            echo "$1 needs a value." >&2
            exit 1
        fi
        NAME="$2"
        shift 2
        ;;
    --android-sdk)
        if [[ $# -lt 2 ]]; then
            echo "$1 needs a value." >&2
            exit 1
        fi
        ANDROID_SDK="$2"
        shift 2
        ;;
    --java-home)
        if [[ $# -lt 2 ]]; then
            echo "$1 needs a value." >&2
            exit 1
        fi
        JAVA_HOME_ARG="$2"
        shift 2
        ;;
    -h | --help | help)
        usage
        exit 0
        ;;
    *)
        echo "Unknown argument: $1" >&2
        usage >&2
        exit 1
        ;;
    esac
done

if [[ ! "${NAME}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || [[ "${NAME}" == "." || "${NAME}" == ".." ]]; then
    echo "Bundle name must contain only letters, numbers, dots, underscores, and hyphens." >&2
    exit 1
fi

if [[ "$(uname -m)" != "x86_64" ]]; then
    echo "This script currently produces only the explicitly labelled linux-x64 bundle." >&2
    exit 1
fi

BUNDLE_DIR="${RELEASE_ROOT}/${NAME}"
ARCHIVE_PATH="${RELEASE_ROOT}/${NAME}-linux-x64.tar.gz"

if [[ -e "${BUNDLE_DIR}" ]]; then
    echo "Experimental bundle already exists: ${BUNDLE_DIR}" >&2
    exit 1
fi
if [[ -e "${ARCHIVE_PATH}" ]]; then
    echo "Experimental archive already exists: ${ARCHIVE_PATH}" >&2
    exit 1
fi

BUILD_ANDROID=0
if [[ -n "${ANDROID_SDK}" ]]; then
    if [[ -d "${ANDROID_SDK}" ]]; then
        BUILD_ANDROID=1
    else
        echo "Android SDK path '${ANDROID_SDK}' does not exist; skipping the APK build." >&2
    fi
else
    echo "No Android SDK given (--android-sdk / ANDROID_SDK_ROOT / ANDROID_HOME); skipping the APK build." >&2
fi

echo "== Building native-host (release) =="
(
    cd "${PROJECT_ROOT}/native-host"
    cargo fmt --all -- --check
    cargo test --locked --all-targets
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --release --lib network::tests::loopback_fault_recovery_stays_inside_one_120_hz_frame -- --ignored --exact
    cargo test --locked --release --lib v5_host::gameplay_tests::production_loopback_latency -- --ignored --exact --nocapture --test-threads=1
    cargo build --locked --release
)

echo "== Building the Tauri launcher (release) =="
(
    cd "${PROJECT_ROOT}/tauri-launcher"
    npm ci --no-audit --no-fund
    npm audit --audit-level=high
    npm test
    npm run build
    cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
    cargo test --locked --manifest-path src-tauri/Cargo.toml --all-targets
    cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
    npx --no-install tauri build --no-bundle
)

APK_SRC=""
if [[ "${BUILD_ANDROID}" -eq 1 ]]; then
    echo "== Building the Android APK (release) =="
    if [[ -z "${JAVA_HOME_ARG}" ]]; then
        echo "JAVA_HOME not set and --java-home not given; skipping the APK build." >&2
        BUILD_ANDROID=0
    else
        ANDROID_DIR="${PROJECT_ROOT}/android-app"
        (
            cd "${ANDROID_DIR}"
            JAVA_HOME="${JAVA_HOME_ARG}" ANDROID_HOME="${ANDROID_SDK}" ANDROID_SDK_ROOT="${ANDROID_SDK}" \
                ./gradlew --no-daemon \
                -PholodoriVersionName="${VERSION}" \
                -PholodoriVersionCode=26 \
                clean testDebugUnitTest assembleDebug assembleRelease lintDebug lintRelease
        )
        APK_SRC="${ANDROID_DIR}/app/build/outputs/apk/release/app-release.apk"
        if [[ ! -f "${APK_SRC}" ]]; then
            echo "Android release build did not produce ${APK_SRC}." >&2
            exit 1
        fi
        BUILD_TOOLS_DIR="$(find "${ANDROID_SDK}/build-tools" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -n1)"
        if [[ -z "${BUILD_TOOLS_DIR}" || ! -x "${BUILD_TOOLS_DIR}/apksigner" ]]; then
            echo "Could not find an executable apksigner under ${ANDROID_SDK}/build-tools." >&2
            exit 1
        fi
        "${BUILD_TOOLS_DIR}/apksigner" verify --verbose "${APK_SRC}"
    fi
fi

echo "== Assembling the bundle =="
LINUX_DIR="${BUNDLE_DIR}/Linux"
DOCS_DIR="${BUNDLE_DIR}/Docs"
mkdir -p "${LINUX_DIR}" "${DOCS_DIR}"

NATIVE_RELEASE="${PROJECT_ROOT}/native-host/target/release"
TAURI_RELEASE="${PROJECT_ROOT}/tauri-launcher/src-tauri/target/release"

cp "${NATIVE_RELEASE}/holodori-native-host" "${LINUX_DIR}/"
cp "${TAURI_RELEASE}/holodori-usb-controller" "${BUNDLE_DIR}/HolodoriUsbController"
cp "${PROJECT_ROOT}/packaging/experimental/README-linux.txt" "${BUNDLE_DIR}/README.txt"
cp "${PROJECT_ROOT}/packaging/experimental/run-keys.sh" "${BUNDLE_DIR}/"
chmod +x "${LINUX_DIR}/holodori-native-host" "${BUNDLE_DIR}/HolodoriUsbController" "${BUNDLE_DIR}/run-keys.sh"
cp "${PROJECT_ROOT}/EXPERIMENTAL_ARCHITECTURE.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/LINUX_SETUP.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/PROTOCOL_V5.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/PROTOCOL_V5_TEST_VECTORS.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/PROTOCOL_V4.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/LATENCY_VALIDATION.md" "${DOCS_DIR}/"
cp "${PROJECT_ROOT}/LICENSE" "${DOCS_DIR}/"

if [[ "${BUILD_ANDROID}" -eq 1 ]]; then
    ANDROID_OUTPUT_DIR="${BUNDLE_DIR}/Android"
    mkdir -p "${ANDROID_OUTPUT_DIR}"
    cp "${APK_SRC}" "${ANDROID_OUTPUT_DIR}/Doritrack-v5.apk"
fi

BRANCH="$(git -C "${PROJECT_ROOT}" branch --show-current)"
COMMIT="$(git -C "${PROJECT_ROOT}" rev-parse HEAD)"
if [[ -n "$(git -C "${PROJECT_ROOT}" status --porcelain --untracked-files=no)" ]]; then
    DIRTY="yes"
else
    DIRTY="no"
fi
{
    echo "name=${NAME}"
    if [[ "${BUILD_ANDROID}" -eq 1 ]]; then
        echo "android_version_name=${VERSION}"
        echo "android_version_code=26"
    fi
    echo "built_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "branch=${BRANCH}"
    echo "base_commit=${COMMIT}"
    echo "working_tree_dirty=${DIRTY}"
    echo "transport=explicit-usb-tether-or-local-network-udp"
    echo "udp_port=42825"
    echo "protocol=5"
    echo "legacy_protocol=4-usb-explicit"
    echo "linux_arch=x86_64"
    echo "linux_launcher=tauri"
    echo "linux_webview=webkit2gtk-system"
    if [[ "${BUILD_ANDROID}" -eq 1 ]]; then
        echo "android_signing=debug-key experimental"
    fi
} >"${BUNDLE_DIR}/BUILD-INFO.txt"

echo "== Writing checksums =="
CHECKSUM_PATH="${BUNDLE_DIR}/SHA256SUMS.txt"
(
    cd "${BUNDLE_DIR}"
    find . -type f ! -name "SHA256SUMS.txt" -print0 |
        sort -z |
        xargs -0 sha256sum |
        sed 's#\./##' >"${CHECKSUM_PATH}"
)

echo "== Archiving =="
mkdir -p "${RELEASE_ROOT}"
tar --create --gzip --file "${ARCHIVE_PATH}" --directory "${RELEASE_ROOT}" "${NAME}"

git -C "${PROJECT_ROOT}" diff --check

echo "Bundle: ${BUNDLE_DIR}"
echo "Archive: ${ARCHIVE_PATH}"
if [[ "${BUILD_ANDROID}" -eq 1 ]]; then
    echo "Android package: ${ANDROID_OUTPUT_DIR}/Doritrack-v5.apk"
fi
