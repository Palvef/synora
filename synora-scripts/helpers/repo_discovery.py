"""Discover repository versions from upstream indexes; never invent a fallback list."""
import argparse
import csv
import io
import datetime
import xml.etree.ElementTree as ET
import functools
import re
import sys
from html.parser import HTMLParser
from urllib.parse import urljoin, urlsplit, unquote
import requests

class Links(HTMLParser):
    def __init__(self):
        super().__init__(); self.links=[]
    def handle_starttag(self, tag, attrs):
        if tag == 'a': self.links.extend(v for k,v in attrs if k == 'href' and v)

@functools.lru_cache(maxsize=512)
def directories(url):
    url=url.rstrip('/')+'/'
    host=urlsplit(url).hostname or ''
    if host.endswith('.github.io'):
        owner=host.removesuffix('.github.io');parts=urlsplit(url).path.strip('/').split('/')
        r=requests.get(f'https://api.github.com/repos/{owner}/{parts[0]}/contents/'+ '/'.join(parts[1:]),timeout=(30,60));r.raise_for_status()
        names=[item['name'] for item in r.json() if item.get('type')=='dir']
        if not names:raise RuntimeError(f'No GitHub Pages directories at {url}')
        return sorted(names)
    if urlsplit(url).hostname == 'repo.mongodb.org':
        prefix=urlsplit(url).path.lstrip('/')
        names=[]; token=None
        for _ in range(100):
            params={'list-type':'2','delimiter':'/','prefix':prefix}
            if token: params['continuation-token']=token
            r=requests.get('https://s3.amazonaws.com/repo.mongodb.org',params=params,timeout=(30,60));r.raise_for_status()
            doc=ET.fromstring(r.content);ns={'s':'http://s3.amazonaws.com/doc/2006-03-01/'}
            names.extend(n.text[len(prefix):].rstrip('/') for n in doc.findall('s:CommonPrefixes/s:Prefix',ns))
            if doc.findtext('s:IsTruncated',namespaces=ns)!='true':break
            token=doc.findtext('s:NextContinuationToken',namespaces=ns)
            if not token:raise RuntimeError('Missing S3 continuation token')
        else:raise RuntimeError('S3 discovery pagination limit reached')
        if not names:raise RuntimeError(f'Empty MongoDB directory: {url}')
        return sorted(set(names))
    r=requests.get(url,timeout=(30,60)); r.raise_for_status()
    parser=Links(); parser.feed(r.text)
    names=set()
    for href in parser.links:
        target=urlsplit(urljoin(url,href)); base=urlsplit(url)
        if target.netloc != base.netloc or target.query or not target.path.startswith(base.path): continue
        name=unquote(target.path[len(base.path):]).rstrip('/')
        if re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+-]*',name) and target.path.endswith('/'): names.add(name)
    if not names: raise RuntimeError(f'No repository directories discovered at {url}; refusing an empty sync')
    return sorted(names)

def expand_path(base, pattern):
    """Expand @auto at directory boundaries, preserving safe relative paths."""
    paths=['']
    for part in pattern.split('/'):
        new=[]
        for prefix in paths:
            if '@auto' in part:
                rx=re.compile(re.escape(part).replace('@auto',r'([A-Za-z0-9][A-Za-z0-9._+-]*)')+'$')
                values=[x for x in directories(base.rstrip('/')+'/'+prefix) if rx.fullmatch(x)]
            else: values=[part]
            new.extend(prefix+x+'/' for x in values)
        paths=new
    if not paths: raise RuntimeError(f'No versions matched {pattern} at {base}')
    return [p.rstrip('/') for p in paths]

@functools.lru_cache(maxsize=8)
def distro_versions(template):
    distro='ubuntu' if template=='ubuntu-lts' else 'debian'
    url=f'https://salsa.debian.org/debian/distro-info-data/-/raw/main/{distro}.csv'
    r=requests.get(url,timeout=(30,60));r.raise_for_status()
    today=datetime.date.today().isoformat()
    rows=[row for row in csv.DictReader(io.StringIO(r.text)) if row.get('release') and row['release']<=today]
    if distro=='ubuntu':rows=[row for row in rows if 'LTS' in row['version']][-3:]
    else:
        rows=[row for row in rows if (row.get('eol-lts') or row.get('eol') or '9999-12-31')>=today]
        if template=='debian-latest':rows=rows[-1:]
        elif template=='debian-latest2':rows=rows[-2:]
    values=[row['series'] for row in rows]
    if not values:raise RuntimeError(f'No maintained releases from {url}')
    return values

@functools.lru_cache(maxsize=4)
def rpm_versions(template):
    if template == '@fedora-current':
        r=requests.get('https://bodhi.fedoraproject.org/releases/',params={'state':'current'},timeout=(30,60));r.raise_for_status()
        values=sorted({str(v['version']) for v in r.json()['releases'] if str(v.get('version','')).isdigit() and str(v.get('name','')).startswith('F')},key=int)
    elif template == '@rhel-current':
        values=[v for v in directories('https://repo.almalinux.org/almalinux/') if v.isdigit()]
        values=sorted(set(values),key=int)
    else:raise ValueError(f'Unknown release template: {template}')
    if not values:raise RuntimeError(f'No current RPM releases for {template}')
    return values

def apt_suites(base,patterns):
    result=[]
    for pattern in patterns.split(','):
        match=re.search(r'@\{(ubuntu-lts|debian-current|debian-latest2|debian-latest)\}|@(ubuntu-lts|debian-current|debian-latest2|debian-latest)',pattern)
        if match:
            result.extend(pattern[:match.start()]+v+pattern[match.end():] for v in distro_versions(match.group(1) or match.group(2)))
        else: result.extend(expand_path(base.rstrip('/')+'/dists',pattern) if '@auto' in pattern else [pattern])
    if not result:raise RuntimeError('No APT suites discovered')
    return list(dict.fromkeys(result))

@functools.lru_cache(maxsize=512)
def release_fields(base,suite):
    url=base.rstrip('/')+'/dists/'+suite+'/Release'
    r=requests.get(url,timeout=(30,60))
    if r.status_code==404:
        r=requests.get(base.rstrip('/')+'/dists/'+suite+'/InRelease',timeout=(30,60))
    r.raise_for_status()
    fields=dict(re.findall(r'^([A-Za-z][A-Za-z-]*):[ \t]*(.*)$',r.text,re.M))
    if not fields.get('Components') or not fields.get('Architectures'): raise RuntimeError(f'Invalid Release metadata: {url}')
    for field in ('Components','Architectures'):
        if any(not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+/-]*',v) or '..' in v.split('/') for v in fields[field].split()):
            raise RuntimeError(f'Unsafe Release {field}: {url}')
    return fields

def elastic_versions():
    r=requests.get('https://artifacts-api.elastic.co/v1/versions',timeout=(30,60));r.raise_for_status()
    majors=sorted({int(v.split('.')[0]) for v in r.json()['versions'] if re.fullmatch(r'\d+\.\d+\.\d+',v)})
    if not majors: raise RuntimeError('Elastic API returned no stable versions')
    return [f'{v}.x' for v in majors]

if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('kind',choices=['dirs','elastic','distros']);p.add_argument('url',nargs='?');p.add_argument('--pattern',default='.*');a=p.parse_args()
    try:
        values=elastic_versions() if a.kind=='elastic' else (distro_versions(a.url) if a.kind=='distros' else [x for x in directories(a.url) if re.fullmatch(a.pattern,x)])
        if not values: raise RuntimeError('Version discovery returned no matches')
        print('\n'.join(values))
    except Exception as e: print(f'Version discovery failed: {e}',file=sys.stderr);sys.exit(1)
