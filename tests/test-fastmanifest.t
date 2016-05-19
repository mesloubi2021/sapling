Setup

  $ PYTHONPATH=$TESTDIR/..:$PYTHONPATH
  $ export PYTHONPATH

Check diagnosis, debugging information
1) Setup configuration
  $ mkcommit() {
  >    echo "$1" > "$1"
  >    hg add "$1"
  >    echo "add $1" > msg
  >    echo "" >> msg
  >    hg ci -l msg
  > }

2) Check access pattern

  $ printaccessedrevs() {
  >     [ ! -f "$TESTTMP/logfile" ] && echo "no access" && return
  >     python "$TESTTMP/summary.py" "$TESTTMP/cachedrevs" "$TESTTMP/logfile"
  >     rm "$TESTTMP/logfile"
  > }

  $ savecachedrevs() {
  >      (printf "%d " "-1"
  >       hg log -r "fastmanifesttocache()" -T "{rev} "
  >       echo "") > $TESTTMP/cachedrevs
  > }


  $ cat > $TESTTMP/summary.py << EOM
  > import sys
  > def summary(cached,accessed):
  >     accessed = [line.strip() for line in open(accessed).readlines()]
  >     cached = open(cached).readlines()[0]
  >     accessedset = set(accessed)
  >     cachedset = set(cached.strip().split(' '))
  >     print '================================================='
  >     print 'CACHE MISS %s' % sorted(accessedset - cachedset)
  >     print 'CACHE HIT %s' % sorted(accessedset & cachedset)
  >     print '================================================='
  > summary(sys.argv[1], sys.argv[2])
  > EOM

  $ clearlogs() {
  >   rm "$TESTTMP/logfile"
  > }

  $ mkdir accesspattern
  $ cd accesspattern
  $ hg init
  $ cat >> .hg/hgrc << EOF
  > [extensions]
  > fastmanifest=
  > [fastmanifest]
  > logfile=$TESTTMP/logfile
  > EOF

2a) Commit

  $ savecachedrevs
  $ mkcommit a

  $ savecachedrevs
  $ mkcommit b
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['-1', '0']
  =================================================

  $ echo "c" > a
  $ savecachedrevs
  $ hg commit -m "new a"
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['-1', '1']
  =================================================

2b) Diff

  $ savecachedrevs
  $ hg diff -c . > /dev/null
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['1', '2']
  =================================================

  $ savecachedrevs
  $ hg diff -c ".^" > /dev/null
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['0', '1']
  =================================================

  $ savecachedrevs
  $ hg diff -r ".^" > /dev/null
  $ clearlogs

2c) Log (TODO)

2d) Update

  $ savecachedrevs
  $ hg update ".^^" -q
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['0', '2']
  =================================================

  $ savecachedrevs
  $ hg update tip -q
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['0', '2']
  =================================================

2e) Rebase
  $ mkcommit c
  $ mkcommit d
  $ hg update ".^^" -q
  $ mkcommit e
  created new head
  $ mkcommit f
  $ hg log -G -r 0:: -T "{rev} {node} {desc|firstline}"
  @  6 dd82c74514cbce45a3c61caf7ffaba16de19cec4 add f
  |
  o  5 5234b99c4f1d5b2ea45ea608550c66015f8f37ac add e
  |
  | o  4 cab0f51bb3f5493da8e7406e3967ef925e2e7a1f add d
  | |
  | o  3 329ad08f9742620b0b3be4305ca0c911d5517e84 add c
  |/
  o  2 00e42334abdae99958cd58b9be90fc940ca2b491 new a
  |
  o  1 7c3bad9141dcb46ff89abf5f61856facd56e476c add b
  |
  o  0 1f0dee641bb7258c56bd60e93edfa2405381c41e add a
  


  $ savecachedrevs
  $ printaccessedrevs
  =================================================
  CACHE MISS []
  CACHE HIT ['-1', '2', '3', '4', '5']
  =================================================
  $ hg rebase -r 5:: -d 4 --config extensions.rebase=
  rebasing 5:5234b99c4f1d "add e"
  rebasing 6:dd82c74514cb "add f" (tip)
  saved backup bundle to $TESTTMP/accesspattern/.hg/strip-backup/5234b99c4f1d-c2e049ad-backup.hg (glob)
  $ printaccessedrevs
  =================================================
  CACHE MISS ['7', '8']
  CACHE HIT ['-1', '2', '4', '5', '6']
  =================================================

  $ cd ..

3) Basic cache testing

  $ mkdir cachetesting
  $ cd cachetesting
  $ hg init
  $ cat >> .hg/hgrc << EOF
  > [extensions]
  > fastmanifest=
  > EOF

  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ mkcommit d
  $ mkcommit e
  $ hg diff -c . --debug --nodate
  cache miss for fastmanifest f064a7f8e3e138341587096641d86e9d23cd9778
  cache miss for fastmanifest 7ab5760d084a24168f7595c38c00f4bbc2e308d9
  performing diff
  diff: other side is hybrid manifest
  diff: cache miss
  diff -r 47d2a3944de8b013de3be9578e8e344ea2e6c097 -r 9d206ffc875e1bc304590549be293be36821e66c e
  --- /dev/null
  +++ b/e
  @@ -0,0 +1,1 @@
  +e

  $ hg debugcachemanifest -a --debug
  caching rev: <addset <baseset+ [0, 1, 2, 3, 4]>, <baseset+ []>>, synchronous(False)
  caching revision a0c8bcbbb45c63b90b70ad007bf38961f64f2af0
  cache miss for fastmanifest a0c8bcbbb45c63b90b70ad007bf38961f64f2af0
  caching revision a539ce0c1a22b0ecf34498f9f5ce8ea56df9ecb7
  cache miss for fastmanifest a539ce0c1a22b0ecf34498f9f5ce8ea56df9ecb7
  caching revision e3738bf5439958f89499a656982023aba57b076e
  cache miss for fastmanifest e3738bf5439958f89499a656982023aba57b076e
  caching revision f064a7f8e3e138341587096641d86e9d23cd9778
  cache miss for fastmanifest f064a7f8e3e138341587096641d86e9d23cd9778
  caching revision 7ab5760d084a24168f7595c38c00f4bbc2e308d9
  cache miss for fastmanifest 7ab5760d084a24168f7595c38c00f4bbc2e308d9

  $ hg diff -c . --debug --nodate
  cache hit for fastmanifest f064a7f8e3e138341587096641d86e9d23cd9778
  cache hit for fastmanifest 7ab5760d084a24168f7595c38c00f4bbc2e308d9
  performing diff
  diff: other side is hybrid manifest
  diff -r 47d2a3944de8b013de3be9578e8e344ea2e6c097 -r 9d206ffc875e1bc304590549be293be36821e66c e
  --- /dev/null
  +++ b/e
  @@ -0,0 +1,1 @@
  +e

Test the --pruneall command to prune all the cached manifests
  $ hg debugcachemanifest --pruneall --debug
  caching rev: [], synchronous(False)
  removing cached manifest fast7ab5760d084a24168f7595c38c00f4bbc2e308d9
  removing cached manifest fastf064a7f8e3e138341587096641d86e9d23cd9778
  removing cached manifest faste3738bf5439958f89499a656982023aba57b076e
  removing cached manifest fasta539ce0c1a22b0ecf34498f9f5ce8ea56df9ecb7
  removing cached manifest fasta0c8bcbbb45c63b90b70ad007bf38961f64f2af0

  $ hg diff -c . --debug --nodate
  cache miss for fastmanifest f064a7f8e3e138341587096641d86e9d23cd9778
  cache miss for fastmanifest 7ab5760d084a24168f7595c38c00f4bbc2e308d9
  performing diff
  diff: other side is hybrid manifest
  diff: cache miss
  diff -r 47d2a3944de8b013de3be9578e8e344ea2e6c097 -r 9d206ffc875e1bc304590549be293be36821e66c e
  --- /dev/null
  +++ b/e
  @@ -0,0 +1,1 @@
  +e

  $ cd ..
