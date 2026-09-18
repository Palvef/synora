// PEP 503 normalization, PEP 691 negotiation and Shadowmire on-disk layout.
// Compatible with njs 0.7.9: do not use replaceAll or newer language APIs.
function normalize(name) {
    return name.toLowerCase().replace(/[-_.]+/g, '-');
}
function representation(accept) {
    // Honor q=0 and client preference; prefer JSON when equally acceptable.
    var candidates = [
        ['application/vnd.pypi.simple.v1+json', 'v1_json'],
        ['application/vnd.pypi.simple.v1+html', 'v1_html'],
        ['text/html', 'html']
    ];
    if (!accept) return 'html';
    var ranges = accept.toLowerCase().split(',');
    var best = '', bestQuality = -1;
    for (var i = 0; i < candidates.length; i++) {
        var specificity = -1, quality = 0;
        for (var j = 0; j < ranges.length; j++) {
            var parts = ranges[j].trim().split(';');
            var media = parts[0].trim();
            var spec = media === candidates[i][0] ? 2 :
                media === candidates[i][0].split('/')[0] + '/*' ? 1 : media === '*/*' ? 0 : -1;
            if (spec < 0 || spec < specificity) continue;
            var q = 1;
            for (var k = 1; k < parts.length; k++) {
                var parameter = parts[k].trim();
                if (parameter.indexOf('q=') === 0) {
                    q = Number(parameter.substring(2));
                    if (!isFinite(q) || q < 0 || q > 1) q = 0;
                }
            }
            if (spec > specificity || q > quality) { specificity = spec; quality = q; }
        }
        if (quality > 0 && quality > bestQuality) { best = candidates[i][1]; bestQuality = quality; }
    }
    return best;
}
function route(r) {
    var uri = r.uri;
    // Nginx already decodes and normalizes URI paths; never expose other files.
    if (uri === '/pypi' || uri === '/pypi/') return { redirect: '/pypi/simple/' };
    if (uri === '/pypi/web') return { redirect: '/pypi/simple/' };
    if (uri.indexOf('/pypi/web/') === 0) {
        return { redirect: '/pypi/' + uri.substring('/pypi/web/'.length) };
    }
    var rootIndex = /^\/pypi\/simple\/index\.(html|v1_html|v1_json)$/.exec(uri);
    if (rootIndex) return { file: uri, simple: true };
    var simple = /^\/pypi\/simple(?:\/([A-Za-z0-9][A-Za-z0-9._-]*))?\/?$/.exec(uri);
    if (simple) {
        var canonical = '/pypi/simple/' + (simple[1] ? normalize(simple[1]) + '/' : '');
        if (uri !== canonical) return { redirect: canonical };
        var suffix = representation(r.headersIn.Accept || r.headersIn.accept || '');
        return suffix ? { file: canonical + 'index.' + suffix, simple: true } : { unacceptable: true };
    }
    var json = /^\/pypi\/([A-Za-z0-9][A-Za-z0-9._-]*)\/json\/?$/.exec(uri);
    if (!json) json = /^\/pypi\/json\/([A-Za-z0-9][A-Za-z0-9._-]*)\/?$/.exec(uri);
    if (json) {
        var jsonPath = '/pypi/json/' + normalize(json[1]);
        if (uri !== jsonPath) return { redirect: jsonPath };
        return { file: jsonPath };
    }
    // PyPI blob URLs are two hash prefixes, a hash directory and a filename.
    // Restrict every component so a miss cannot redirect arbitrary URLs/paths.
    var blob = /^\/pypi\/packages\/[0-9a-f]{2}\/[0-9a-f]{2}\/[0-9a-f]+\/[A-Za-z0-9][A-Za-z0-9._+!-]*$/.exec(uri);
    if (blob && !/\.tmp$/.test(uri)) return { file: uri, package: true };
    return {};
}
function redirect(r) { return route(r).redirect || ''; }
function allowed(r) { var v = route(r); return v.file || v.redirect || v.unacceptable ? '1' : '0'; }
function unacceptable(r) { return route(r).unacceptable ? '1' : '0'; }
function file(r) { return route(r).file || ''; }
function upstreamPath(r) { return '/pypi/web/' + (route(r).file || '').substring('/pypi/'.length); }
function logUrl(r) {
    // Use the public request path, never the internally selected index filename.
    return r.variables.request_uri.split('?')[0].replace(/^\/pypi\/web\//, '/pypi/');
}
function missing(r) {
    var v = route(r);
    if (v.package) {
        r.return(302, 'https://mirrors.tuna.tsinghua.edu.cn/pypi/web/packages/' +
            v.file.substring('/pypi/packages/'.length));
    } else {
        r.return(404, 'PyPI metadata not found\n');
    }
}
export default { normalize, representation, route, redirect, allowed, unacceptable, file, logUrl, upstreamPath, missing };
