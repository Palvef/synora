#!/usr/bin/env python3
"""OpenSSL adapter for rsync-ssl using Synora's injected HTTP CONNECT proxy."""
import os
import sys
from urllib.parse import unquote, urlsplit


def command(arguments, environment):
    proxy = next((environment[key] for key in (
        'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy', 'ALL_PROXY', 'all_proxy'
    ) if environment.get(key)), '')
    parsed = urlsplit(proxy)
    if parsed.scheme != 'http' or not parsed.hostname:
        raise ValueError('rsync-ssl requires an injected HTTP CONNECT proxy; direct fallback is disabled')
    host = parsed.hostname
    if ':' in host:
        host = f'[{host}]'
    options = ['-proxy', f'{host}:{parsed.port or 80}']
    child_environment = environment.copy()
    if parsed.username is not None:
        options += ['-proxy_user', unquote(parsed.username), '-proxy_pass', 'env:SYNORA_RSYNC_PROXY_PASSWORD']
        child_environment['SYNORA_RSYNC_PROXY_PASSWORD'] = unquote(parsed.password or '')
    if not arguments or arguments[0] != 's_client':
        raise ValueError('Only OpenSSL s_client is supported')
    return ['openssl', arguments[0], *options, *arguments[1:]], child_environment


if __name__ == '__main__':
    try:
        arguments, environment = command(sys.argv[1:], os.environ)
    except ValueError as error:
        print(f'rsync-ssl proxy setup failed: {error}', file=sys.stderr)
        sys.exit(1)
    os.execvpe(arguments[0], arguments, environment)
