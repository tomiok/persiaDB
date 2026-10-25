import sys, math
from PIL import Image, ImageDraw, ImageFont
W,H,FPS,DUR=1920,1080,30,16
PAPER=(243,244,246);INK=(27,30,35);MUTED=(62,70,82);LINE=(207,211,218);WASH=(230,232,236);ACC=(12,92,107);ACC2=(142,58,38);GRAY=(107,114,128)
FP="/usr/share/fonts/truetype/sand-box/google/JetBrains Mono/JetBrainsMono-VariableFont_wght.ttf"
_c={}
def F(s,w="Regular"):
    k=(s,w)
    if k not in _c:
        f=ImageFont.truetype(FP,s)
        try:f.set_variation_by_name(w)
        except Exception:pass
        _c[k]=f
    return _c[k]
def cl(x):return max(0,min(1,x))
def ease(x):x=cl(x);return x*x*(3-2*x)
def eo(x):x=cl(x);return 1-(1-x)**3
def mix(a,b,t):return tuple(int(a[i]+(b[i]-a[i])*t) for i in range(3))
def seg(t,a,b):return cl((t-a)/(b-a))
def txt(d,xy,s,f,c,a=1,anchor="la"):
    d.text(xy,s,font=f,fill=mix(PAPER,c,a),anchor=anchor)
def grid(d):
    for x in range(0,W,80):d.line([(x,0),(x,H)],fill=mix(PAPER,LINE,.35))
    for y in range(0,H,80):d.line([(0,y),(W,y)],fill=mix(PAPER,LINE,.35))
def wordmark(d,cx,cy,a,typed=1.0,size=120,cursor=True,t=0):
    s="Persia DB";n=int(round(len(s)*typed));f=F(size,"Bold")
    full=d.textlength(s,font=f);x0=cx-full/2
    # logo box
    b=size*1.05;bx=x0-b-40
    d.rectangle([bx,cy-b/2,bx+b,cy+b/2],fill=mix(PAPER,INK,a))
    txt(d,(bx+b/2,cy),"P",F(int(size*.6),"Bold"),PAPER,1,"mm") if a>.5 else None
    txt(d,(x0,cy),s[:n],f,INK,a,"lm")
    if cursor and (int(t*2)%2==0 or typed<1):
        cx2=x0+d.textlength(s[:n],font=f)+8
        d.rectangle([cx2,cy-size*.45,cx2+size*.08,cy+size*.45],fill=mix(PAPER,ACC,a))
def frame(i):
    t=i/FPS
    im=Image.new("RGB",(W,H),PAPER);d=ImageDraw.Draw(im);grid(d)
    # Scene1 0-3.4 wordmark
    if t<3.6:
        a=ease(seg(t,0.1,0.6))*(1-ease(seg(t,3.0,3.5)))
        dy=-30*ease(seg(t,3.0,3.5))
        wordmark(d,W/2+60,H/2-40+dy,a,eo(seg(t,0.3,1.3)),t=t)
        ta=ease(seg(t,1.4,2.0))*(1-ease(seg(t,3.0,3.5)))
        txt(d,(W/2,H/2+80+dy),"Think SQLite, but for search.",F(40),MUTED,ta,"mm")
        txt(d,(W/2,H/2+140+dy),"embedded · single-file · written in Rust",F(28),ACC,ta,"mm")
    # Scene2 3.5-9 terminal
    if 3.4<t<9.4:
        a=ease(seg(t,3.5,4.0))*(1-ease(seg(t,8.8,9.3)))
        oy=40*(1-eo(seg(t,3.5,4.1)))
        x0,y0,x1,y1=310,230+oy,1610,820+oy
        d.rectangle([x0,y0,x1,y1],fill=mix(PAPER,(255,255,255),a),outline=mix(PAPER,INK,a),width=2)
        d.rectangle([x0,y0,x1,y0+52],fill=mix(PAPER,WASH,a),outline=mix(PAPER,INK,a),width=2)
        for k,c in enumerate([ACC2,GRAY,ACC]):
            d.rectangle([x0+22+k*30,y0+18,x0+38+k*30,y0+34],fill=mix(PAPER,c,a))
        txt(d,((x0+x1)/2,y0+26),"~/my-app — main.rs",F(22),MUTED,a,"mm")
        f=F(30);lx=x0+50;ly=y0+100;lh=52
        code=['let db = Db::open("app.persia")?;','let hits = db.search("fast rust serach")?;']
        ts=[(4.0,5.0),(5.1,6.3)]
        for k,(s,(p,q)) in enumerate(zip(code,ts)):
            n=int(len(s)*cl((t-p)/(q-p)))
            if t>p:
                txt(d,(lx,ly+k*lh),s[:n],f,INK,a)
                if t<q+.3 and (k==1 or t<q):
                    cx=lx+d.textlength(s[:n],font=f)
                    if t<q or int(t*3)%2==0:d.rectangle([cx+2,ly+k*lh+4,cx+18,ly+k*lh+38],fill=mix(PAPER,ACC,a))
        res=[("0.94","Fast full-text search for Rust apps"),("0.81","Embedded search engine, no server needed"),("0.67","Rust: ranking documents with BM25F"),("0.52","Searching a single file, like SQLite")]
        ry=ly+2*lh+40
        ha=ease(seg(t,6.5,6.8))
        txt(d,(lx,ry),'→ 4 results · relaxed: "serach" → "search"',F(26),ACC,a*ha)
        d.line([(lx,ry+48),(x1-50,ry+48)],fill=mix(PAPER,LINE,a*ha),width=2)
        for k,(sc,ti) in enumerate(res):
            p=6.75+k*0.18;ra=a*ease(seg(t,p,p+.3));dx=20*(1-eo(seg(t,p,p+.4)))
            yy=ry+75+k*62
            txt(d,(lx+dx,yy),f"{k+1}.",F(28),GRAY,ra)
            bw=int(140*float(sc)*eo(seg(t,p+.1,p+.7)))
            d.rectangle([lx+60+dx,yy+8,lx+60+dx+bw,yy+28],fill=mix(PAPER,ACC if k==0 else mix(ACC,PAPER,.45),ra))
            txt(d,(lx+220+dx,yy),sc,F(28,"Bold"),ACC,ra)
            txt(d,(lx+320+dx,yy),ti,F(28),INK,ra)
    # Scene3 9.2-13.2 benefits
    if 9.1<t<13.4:
        out=1-ease(seg(t,12.7,13.2))
        items=[("Embedded.","No server. No cluster."),("Full-text search inside your app,","like SQLite."),("Native Rust.","Go and TypeScript SDKs coming.")]
        for k,(a1,b1) in enumerate(items):
            p=9.3+k*0.9;a=ease(seg(t,p,p+.5))*out;dx=40*(1-eo(seg(t,p,p+.6)))
            y=300+k*170
            d.rectangle([330+dx,y-45,338+dx,y+55],fill=mix(PAPER,ACC if k!=1 else ACC2,a))
            txt(d,(380+dx,y-40),f"0{k+1}",F(26),GRAY,a)
            txt(d,(380+dx,y+20),a1,F(56,"Bold"),INK,a,"lm")
            w=d.textlength(a1+" ",font=F(56,"Bold"))
            txt(d,(380+dx+w,y+20),b1,F(56),MUTED,a,"lm")
    # Scene4 13.2-16 end card
    if t>13.1:
        a=ease(seg(t,13.3,13.8))*(1-ease(seg(t,15.3,15.9)))
        wordmark(d,W/2+60,H/2-60,a,1,size=110,cursor=True,t=t)
        txt(d,(W/2,H/2+60),"Embedded full-text search, written in Rust.",F(36),MUTED,a,"mm")
        ua=ease(seg(t,13.8,14.3))*(1-ease(seg(t,15.3,15.9)))
        s="tomiok.github.io/persiaDB";f=F(32,"Bold");w=d.textlength(s,font=f)
        d.rectangle([W/2-w/2-28,H/2+120,W/2+w/2+28,H/2+190],outline=mix(PAPER,INK,ua),width=2)
        txt(d,(W/2,H/2+155),s,f,ACC,ua,"mm")
    return im
if __name__=="__main__":
    if len(sys.argv)>1:
        for s in sys.argv[1:]:frame(int(float(s)*FPS)).save(f"still_{s}.png")
    else:
        o=sys.stdout.buffer
        for i in range(FPS*DUR):o.write(frame(i).tobytes())
