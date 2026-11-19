# Puts the commands of nx's tool tier on PATH, after the system's own
# directories, so the image and the host tier keep their names. Installed as
# /usr/share/fish/vendor_conf.d/nx.fish: a bootc image's fish reads its
# config from /usr/etc/fish, never /etc/fish/conf.d.
if not contains /var/lib/nx/current/bin $PATH
    set -gx --append PATH /var/lib/nx/current/bin
end
