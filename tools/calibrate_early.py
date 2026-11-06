#!/usr/bin/env python3
"""Calibrate the early trigger (no space needed) on the bundled dictionaries.

Simulates out-of-dictionary words: the dictionary is truncated to the top N
words and the rule is applied to words ranked N..2N (rare words, names,
slang) typed in their own layout — every trigger there is a false positive.
Recall: top-N words of the other language typed in the wrong layout.

Rule: the prefix (>= L letters) starts no word of the typed language, and
words starting with the converted prefix cover >= F of the other language.

    python3 tools/calibrate_early.py [N]
"""
import os
import csv, bisect, sys, collections
R=os.path.join(os.path.dirname(os.path.abspath(__file__)), '..') + '/'
tab=collections.defaultdict(dict)
for row in csv.DictReader(open(R+'data/xkb/us_ru.tsv'),delimiter='\t'):
    tab[row['layout']][(row['keycode'],row['level'])]=row['char']
rev={l:{c:k for k,c in t.items()} for l,t in tab.items()}
def conv(w,s,t):
    out=''
    for ch in w:
        k=rev[s].get(ch)
        if k is None or k not in tab[t]: return None
        out+=tab[t][k]
    return out
def load(f):
    ws=[]
    for line in open(f):
        w,c=line.split(); ws.append((w.lower(),int(c)))
    return ws
EN=load(R+'data/dict/en_50k.txt'); RU=load(R+'data/dict/ru_50k.txt')
class Pref:
    def __init__(s,words):
        d={}
        for w,c in words: d[w]=d.get(w,0)+c
        s.keys=sorted(d); s.cnt=[d[k] for k in s.keys]
        # prefix sums
        s.cum=[0]
        for c in s.cnt: s.cum.append(s.cum[-1]+c)
    def stats(s,p):
        i=bisect.bisect_left(s.keys,p); j=bisect.bisect_left(s.keys,p+'￿')
        return j-i, s.cum[j]-s.cum[i]
N=int(sys.argv[1]) if len(sys.argv)>1 else 25000
lang={'us':EN,'ru':RU}
D={l:Pref(lang[l][:N]) for l in lang}
tot={l:sum(c for _,c in lang[l][:N]) for l in lang}
def fires(typed,S,T,L,F):
    # returns prefix length at which rule fires, else None
    for n in range(L,len(typed)+1):
        p=typed[:n]
        if not any(ch.isalpha() for ch in p): continue
        ns,_=D[S].stats(p)
        if ns: continue
        c=conv(p,S,T)
        if c is None: return None
        nt,ft=D[T].stats(c)
        if nt and ft/tot[T]>=F: return n
        # source impossible but target weak: keep going
    return None
other={'us':'ru','ru':'us'}
for L in [3,4,5]:
  for F in [1e-6,1e-5,1e-4]:
    res=[]
    for S in ['us','ru']:
        T=other[S]
        # FP: rare source words (rank N..2N) typed correctly in S
        rare=[w for w,_ in lang[S][N:2*N] if w.isalpha()]
        fp=sum(1 for w in rare if fires(w,S,T,L,F))
        # recall: target words typed in S layout
        hits=0;wsum=0;pos=0;n=0
        for w,c in lang[T][:N]:
            typed=conv(w,T,S)
            if typed is None or len(w)<L: continue
            n+=1; wsum+=c
            k=fires(typed,S,T,L,F)
            if k: hits+=c; pos+= k/len(w)
        res.append(f"{S}: FP {fp}/{len(rare)}={fp/len(rare):.2%} recall(freq-w) {hits/wsum:.1%}")
    print(f"L={L} F={F:g} | "+" | ".join(res))
