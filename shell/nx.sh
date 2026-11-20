# shellcheck shell=sh
# Puts the commands of nx's tool tier on PATH, after the system's own
# directories, so the image and the host tier keep their names (design,
# *Name ownership*). Installed as /etc/profile.d/nx.sh.
case ":${PATH}:" in
*:/var/lib/nx/current/bin:*) ;;
*) PATH="${PATH:+$PATH:}/var/lib/nx/current/bin" && export PATH ;;
esac
