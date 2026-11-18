(
    set -e
    root=$(mktemp -d)
    trap 'rm -rf "$root"' EXIT
    arch=$(apk --print-arch)
    for index in /var/cache/apk/APKINDEX.*.tar.gz; do
        name=${index##*/}
        repo="$root/${name%.tar.gz}"
        mkdir -p "$repo/$arch"
        cp "$index" "$repo/$arch/APKINDEX.tar.gz"
        echo "$repo" >> "$root/repositories"
    done
    apk --no-network --repositories-file "$root/repositories" update
    apk --no-network --repositories-file "$root/repositories" search busybox > "$root/search"
    apk --no-network --repositories-file "$root/repositories" info -v > "$root/installed"
    test -s "$root/search"
    wc -l "$root/search" "$root/installed"
) && echo APK_LOCAL_DONE
