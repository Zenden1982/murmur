# «Чистый VPS» для прогона deploy/install.sh: systemd, curl, ufw — без Docker (его ставит сам скрипт).
ARG BASE=ubuntu:24.04
FROM ${BASE}
ENV container=docker DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv dbus curl ca-certificates iproute2 ufw iptables procps \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
STOPSIGNAL SIGRTMIN+3
CMD ["/lib/systemd/systemd"]
