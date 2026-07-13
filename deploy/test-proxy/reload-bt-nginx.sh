#!/usr/bin/env bash
set -euo pipefail

nginx_bin=/www/server/nginx/sbin/nginx
nginx_conf=/www/server/nginx/conf/nginx.conf

"${nginx_bin}" -t -c "${nginx_conf}"
"${nginx_bin}" -s reload -c "${nginx_conf}"
