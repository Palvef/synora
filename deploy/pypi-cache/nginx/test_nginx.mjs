import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const source = await readFile(new URL('./synora-pypi.js', import.meta.url), 'utf8');
const { default: pypi } = await import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const request = (uri, accept = '') => ({ uri, headersIn: { Accept: accept } });
assert.equal(pypi.normalize('A...___---B'), 'a-b');
assert.equal(pypi.route(request('/pypi/simple/Foo...__Bar')).redirect, '/pypi/simple/foo-bar/');
assert.equal(pypi.route(request('/pypi/simple/')).file, '/pypi/simple/index.html');
assert.equal(pypi.route(request('/pypi/simple/', 'application/vnd.pypi.simple.v1+json')).file, '/pypi/simple/index.v1_json');
for (const suffix of ['html', 'v1_html', 'v1_json']) {
    assert.equal(pypi.route(request('/pypi/simple/index.' + suffix)).file, '/pypi/simple/index.' + suffix);
}
assert.equal(pypi.representation('application/vnd.pypi.simple.v1+json;q=0,text/html'), 'html');
assert.equal(pypi.representation('application/vnd.pypi.simple.v1+json;q=0,*/*;q=0.5'), 'v1_html');
assert.equal(pypi.representation('application/vnd.pypi.simple.v1+json;q=0.1,text/html;q=0.9'), 'html');
assert.equal(pypi.representation('image/png'), '');
assert.equal(pypi.route(request('/pypi/Foo_Bar/json')).redirect, '/pypi/json/foo-bar');
assert.equal(pypi.route(request('/pypi/json/foo-bar')).file, '/pypi/json/foo-bar');
assert.equal(pypi.route(request('/pypi/web/simple/Foo/')).redirect, '/pypi/simple/Foo/');
const blob = '/pypi/packages/ab/cd/1234/example-1.0-py3-none-any.whl';
assert.equal(pypi.route(request(blob)).file, blob);
let reply;
pypi.missing({ ...request(blob), return: (...args) => { reply = args; } });
assert.deepEqual(reply, [302, 'https://mirrors.tuna.tsinghua.edu.cn/pypi/web/packages/ab/cd/1234/example-1.0-py3-none-any.whl']);
for (const path of ['/pypi/packages/ab/cd/1234/example.whl.tmp', '/pypi/local.db', '/pypi/.yukina/state', '/pypi/json/.hidden', '/pypi/packages/../../local.db', '/pypi/simple/foo/private', '/pypi/packages/ab/cd/1234/evil%0d%0a']) {
    assert.equal(pypi.allowed(request(path)), '0', path);
    pypi.missing({ ...request(path), return: (...args) => { reply = args; } });
    assert.equal(reply[0], 404);
}
console.log('PyPI normalization, negotiation, routing and miss policy: passed');
