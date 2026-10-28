# Rust toolchain, php-cgi, the PHP CLI (Debian trixie: PHP 8.4) and Composer,
# for running RPHP's PHP-dependent tests and trying the server locally. Used
# by scripts/test-php.sh; works with Docker or Podman.
FROM docker.io/library/rust:1-trixie

# HTTPS mirror: some networks block plain-HTTP package downloads.
RUN sed -i "s|http://deb.debian.org|https://deb.debian.org|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends php-cgi php-cli unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL https://getcomposer.org/download/latest-2.x/composer.phar -o /usr/local/bin/composer \
    && chmod +x /usr/local/bin/composer
