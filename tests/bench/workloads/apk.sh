if apk update && apk search busybox > /tmp/apk-search && apk info -v > /tmp/apk-installed; then
    wc -l /tmp/apk-search /tmp/apk-installed
    echo APK_DONE
else
    echo APK_FAILED
fi
