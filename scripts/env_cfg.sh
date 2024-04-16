#!/bin/sh
# Generates pierport config from environment variables

if [ -n "$1" ] && [ "$1" != "-" ]; then
    fd=3
    exec 3> "$1"
else
    fd=1
fi

if [ -n "$PRT_STAGING_DIR" ]; then
    echo "staging_dir = \"$PRT_STAGING_DIR\"" 1>&$fd
fi

if [ -n "$PRT_IMPORTED_DIR" ]; then
    echo "imported_dir = \"$PRT_IMPORTED_DIR\"" 1>&$fd
fi

if [ -n "$PRT_PROXY_IMPORT_BASE" ]; then
    echo "proxy_import_base = \"$PRT_PROXY_IMPORT_BASE\"" 1>&$fd
fi

if [ -n "$PRT_DB_PATH" ]; then
    echo "db_path = \"$PRT_DB_PATH\"" 1>&$fd
fi

if [ -n "$PRT_CACHE_DIR" ]; then
    echo "cache_dir = \"$PRT_CACHE_DIR\"" 1>&$fd
fi

if [ -n "$PRT_BIND" ]; then
    echo "bind = \"$PRT_BIND\"" 1>&$fd
fi

if [ -n "$PRT_MAX_LOOM" ]; then
    echo "max_loom = $PRT_MAX_LOOM" 1>&$fd
fi

if [ -n "$PRT_UPLOAD_LIMIT" ]; then
    echo "upload_limit = $PRT_UPLOAD_LIMIT" 1>&$fd
fi

if [ -n "$PRT_SESSION_CHECK_INTERVAL_SECONDS" ]; then
    echo "session_check_interval_seconds = $PRT_SESSION_CHECK_INTERVAL_SECONDS" 1>&$fd
fi

if [ -n "$PRT_SESSION_REMOVE_TIME_SECONDS" ]; then
    echo "session_remove_time_seconds = $PRT_SESSION_REMOVE_TIME_SECONDS" 1>&$fd
fi

pu_written=0

if [ -n "$PRT_PU_PREP" ]; then
    if [ $pu_written = 0 ]; then
        echo "[post_unpack]" 1>&$fd
        pu_written=1
    fi
    echo "prep = $PRT_PU_PREP" 1>&$fd
fi

if [ -n "$PRT_PU_VERIFY_CRAM" ]; then
    if [ $pu_written = 0 ]; then
        echo "[post_unpack]" 1>&$fd
        pu_written=1
    fi
    echo "verify_cram = $PRT_PU_VERIFY_CRAM" 1>&$fd
fi

if [ -n "$PRT_PU_PACK" ]; then
    if [ $pu_written = 0 ]; then
        echo "[post_unpack]" 1>&$fd
        pu_written=1
    fi
    echo "pack = $PRT_PU_PACK" 1>&$fd
fi

if [ -n "$PRT_PU_MELD" ]; then
    if [ $pu_written = 0 ]; then
        echo "[post_unpack]" 1>&$fd
        pu_written=1
    fi
    echo "meld = $PRT_PU_MELD" 1>&$fd
fi

if [ -n "$PRT_PU_CHOP" ]; then
    if [ $pu_written = 0 ]; then
        echo "[post_unpack]" 1>&$fd
        pu_written=1
    fi
    echo "chop = $PRT_PU_CHOP" 1>&$fd
fi

cap_written=0

if [ -n "$PRT_CAP_INFO" ]; then
    if [ $cap_written = 0 ]; then
        echo "[capabilities]" 1>&$fd
        cap_written=1
    fi
    echo "info = [$PRT_CAP_INFO]" 1>&$fd
fi

if [ -n "$PRT_CAP_EXTENSIONS" ]; then
    if [ $cap_written = 0 ]; then
        echo "[capabilities]" 1>&$fd
        cap_written=1
    fi
    echo "extensions = [$PRT_CAP_EXTENSIONS]" 1>&$fd
fi
