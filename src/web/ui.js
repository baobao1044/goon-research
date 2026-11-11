// goon desktop front-end: intro, streaming transcript, command palette, replay, find, toasts,
// attachments. No framework, no build, no innerHTML for model output.
"use strict";
const D=document,$=id=>D.getElementById(id),qs=s=>D.querySelector(s);
const intro=$("intro"),kinetic=$("kinetic"),shell=$("shell"),T=$("transcript"),empty=$("empty");
const form=$("form"),box=$("prompt"),send=$("send"),stop=$("stop"),counter=$("counter");
const status=$("status"),cwd=$("cwd"),meta=$("meta"),rail=$("sessions");
const MAX=262144,PROMPTS=["Đọc repo này và tóm tắt kiến trúc trong 10 dòng.","Tìm lỗi trong repo, sửa và chạy kiểm tra phù hợp.","Review git diff hiện tại và chỉ ra rủi ro."];
const err=e=>(e&&e.message)||e;
let running=false,turn=null,text="",caret=null,introDone=false,replaying=false,replayId=null,reasoning=true;
let blankAway=false,blankTimer=0,painted=false,abort=null,list=[],stuck=true,unseen=false,jump=null,toasts=null,banner=null,bannerMsg=null,blank=empty;
const n=(tag,cls,txt)=>{const node=D.createElement(tag);if(cls)node.className=cls;if(txt!=null)node.textContent=String(txt);return node;};
const cut=(s,max)=>{const v=String(s??"");return v.length>max?v.slice(0,max-1)+"…":v;};

const CSS=`
.gx-overlay{position:fixed;inset:0;z-index:60;display:none;place-items:start center;background:rgba(4,5,6,.46);padding:11vh 16px 16px}
.gx-overlay.is-open{display:grid;animation:gx-fade var(--t-2) var(--ease-out)}
@keyframes gx-fade{from{opacity:0}to{opacity:1}}
@keyframes gx-in{from{opacity:0;transform:translateY(-8px)}to{opacity:1;transform:translateY(0)}}
@keyframes gx-drain{from{width:100%}to{width:0}}
.gx-panel,.gx-sheet{width:min(640px,100%);background:var(--surface);border:1px solid var(--line-2);animation:gx-in var(--t-2) var(--ease-out)}
.gx-panel{display:flex;flex-direction:column;max-height:min(66vh,560px)}
.gx-sheet{width:min(520px,100%)}
.gx-field,.gx-head{display:flex;align-items:center;gap:10px;padding:11px 14px;border-bottom:1px solid var(--line)}
.gx-head{justify-content:space-between;font:620 13px var(--sans)}
.gx-input{flex:1;min-width:0;background:none;border:0;outline:none;color:var(--ink);font:15px var(--sans)}
.gx-list{overflow-y:auto;padding:6px;display:flex;flex-direction:column;gap:2px}
.gx-item{display:flex;gap:10px;width:100%;padding:9px 10px;border:0;border-left:2px solid transparent;background:none;color:var(--ink-2);font:13px var(--sans);text-align:left;cursor:pointer}
.gx-item.is-active{color:var(--ink);background:var(--surface-2);border-left-color:var(--accent)}
.gx-grid{display:grid;grid-template-columns:auto 1fr}
.gx-grid>*{padding:7px 14px;border-top:1px solid var(--line)}
.gx-grid>:nth-child(odd){font:10.5px var(--mono);color:var(--ink-3);text-align:right;border-right:1px solid var(--line);white-space:nowrap}
.gx-grid>:nth-child(even){font:11.5px var(--mono);color:var(--ink);word-break:break-all}
.gx-toasts{position:fixed;right:18px;bottom:var(--gx-chrome,104px);z-index:70;display:flex;flex-direction:column;gap:8px;align-items:flex-end;max-width:min(360px,calc(100vw - 36px))}
.gx-toast{position:relative;overflow:hidden;background:var(--surface);border:1px solid var(--line-2);border-left:2px solid var(--ink-3);padding:10px 14px 12px;color:var(--ink-2);font-size:12.5px;cursor:pointer;animation:gx-in var(--t-3) var(--ease-out)}
.gx-bar{position:absolute;left:0;bottom:0;height:2px;width:100%;background:var(--accent);animation:gx-drain linear forwards}
.gx-banner{position:fixed;top:0;left:0;right:0;z-index:55;display:none;align-items:center;gap:10px;padding:7px 16px;background:var(--warn);color:#171104;font:11px var(--mono)}
.gx-banner.is-on{display:flex}
.gx-find{position:fixed;top:8px;right:18px;z-index:56;display:none;align-items:center;gap:6px;background:var(--surface);border:1px solid var(--line-2);padding:5px 8px}
.gx-find.is-on{display:flex;animation:gx-in var(--t-2) var(--ease-out)}
.gx-find input{width:168px;background:none;border:0;outline:none;color:var(--ink);font:11.5px var(--mono)}
.gx-find span{font:10px var(--mono);color:var(--ink-3);min-width:44px;text-align:right}
.gx-find button{border:0;background:none;color:var(--ink-3);font:11px var(--mono);padding:2px 5px;cursor:pointer}
mark.gx-hit{background:var(--warn);color:#171104}
mark.gx-hit.is-active{background:var(--accent);color:var(--accent-ink)}
.turn.assistant pre{position:relative}
.gx-copy{position:absolute;top:6px;right:6px;opacity:0;border:1px solid var(--line-2);background:var(--surface-2);color:var(--ink-3);font:9.5px var(--mono);padding:3px 6px;cursor:pointer;transition:opacity var(--t-1) var(--ease-snap)}
pre:hover>.gx-copy,.gx-copy:focus-visible,.gx-copy.is-done{opacity:1;color:var(--accent);border-color:var(--ink-3)}
.session.is-active{border-left-color:var(--accent);color:var(--ink);background:var(--surface-2)}
@media (prefers-reduced-motion:reduce){.gx-overlay.is-open,.gx-panel,.gx-sheet,.gx-toast,.gx-banner.is-on,.gx-find.is-on{animation-duration:1ms!important}}
`;
// CSP allows style-src 'self' only, so an inline <style> would be dropped. A constructable
// stylesheet is CSSOM, not inline style, and still applies; the fallback walks rule by rule.
function adopt(css){
  try{
    const sheet=new CSSStyleSheet();
    sheet.replaceSync(css);
    D.adoptedStyleSheets=[...D.adoptedStyleSheets,sheet];
    return;
  }catch(error){}
  const target=D.styleSheets[0],rules=[];
  let depth=0,buffer="";
  for(const ch of css){
    buffer+=ch;
    if(ch==="{")depth++;
    else if(ch==="}"&&!--depth){rules.push(buffer.trim());buffer="";}
  }
  for(const rule of rules){try{target.insertRule(rule,target.cssRules.length);}catch(error){}}
}
adopt(CSS);

function buildKinetic(){
  let base=0;
  for(const word of kinetic.querySelectorAll(".word")){
    const letters=word.querySelector(".letters"),body=word.dataset.word||"";
    let i=0;
    for(const ch of body){
      const span=n("span","letter",ch);
      span.style.setProperty("--i",String(i));
      span.style.setProperty("--base",base+"ms");
      letters.appendChild(span);
      i++;
    }
    base+=i*26+110;
  }
}
function endIntro(){
  if(introDone)return;
  introDone=true;
  intro.classList.add("is-out");
  shell.classList.remove("is-hidden");
  shell.classList.add("is-in");
  setTimeout(()=>{intro.remove();revealBlank();fit();},220);
  box.focus();
}
for(const type of ["keydown","pointerdown","wheel"])window.addEventListener(type,endIntro,{once:true,passive:true});

function md(node,source){
  node.textContent="";
  const line=(parent,body)=>{
    let last=0;
    for(const hit of body.matchAll(/(`[^`]+`|\*\*[^*]+\*\*)/g)){
      if(hit.index>last)parent.appendChild(D.createTextNode(body.slice(last,hit.index)));
      const token=hit[0];
      parent.appendChild(token[0]==="`"?n("code","",token.slice(1,-1)):n("strong","",token.slice(2,-2)));
      last=hit.index+token.length;
    }
    if(last<body.length)parent.appendChild(D.createTextNode(body.slice(last)));
  };
  String(source??"").split("```").forEach((part,index)=>{
    if(index%2===0){part.split("\n").forEach((text,i)=>{if(i)node.appendChild(D.createTextNode("\n"));line(node,text);});return;}
    const gap=part.indexOf("\n"),pre=D.createElement("pre");
    pre.textContent=(gap<0?"":part.slice(gap+1)).replace(/\n$/,"");
    node.appendChild(pre);
  });
}

// transcript surface: keep the reader's scroll position unless they are already at the bottom
function fit(){
  const r=form&&form.offsetParent?form.getBoundingClientRect():null;
  D.documentElement.style.setProperty("--gx-chrome",(r?Math.max(84,Math.round(innerHeight-r.top+14)):104)+"px");
}
function hideBlank(){
  if(blankAway)return;
  blankAway=true;
  blank.classList.add("is-out");
  clearTimeout(blankTimer);
  blankTimer=setTimeout(()=>{if(blankAway)blank.remove();},180);
}
function revealBlank(){for(const node of blank.querySelectorAll(".reveal"))node.style.opacity="";}
function showBlank(){
  blankAway=false;
  clearTimeout(blankTimer);
  blank.classList.remove("is-out");
  if(!blank.isConnected)T.appendChild(blank);
  revealBlank();
}
function paintJump(){if(jump)jump.style.display=unseen?"block":"none";}
function scrollEnd(force){
  if(replaying&&!force)return;
  if(!force&&!stuck){unseen=true;return paintJump();}
  T.scrollTop=T.scrollHeight;
  unseen=false;
  stuck=true;
  paintJump();
}
T.addEventListener("scroll",()=>{
  if(replaying)return;
  stuck=T.scrollHeight-T.scrollTop-T.clientHeight<48;
  if(stuck)unseen=false;
  paintJump();
},{passive:true});
function attach(node){hideBlank();T.appendChild(node);scrollEnd();return node;}
function addTurn(kind,content){
  const node=n("div","turn "+kind);
  if(kind==="assistant"){md(node,content);node.__raw=String(content??"");}
  else node.textContent=String(content??"");
  return attach(node);
}
function addRow(parts){
  const row=n("div","row");
  for(const part of parts){
    const plain=typeof part==="string";
    row.appendChild(n("span",plain?"":part.className||"",plain?part:part.text));
  }
  attach(row);
}
function addPanel(summary,body,cls){
  const node=n("details",cls?"panel "+cls:"panel");
  node.append(n("summary","",summary),n("pre","",body));
  if(cls&&!reasoning)node.hidden=true;
  attach(node);
}
function setRunning(value){
  running=value;
  send.disabled=value;
  stop.hidden=!value;
  status.textContent=value?"running":"idle";
  status.dataset.state=value?"running":"idle";
}
function startTurn(){
  turn=addTurn("assistant","");
  text="";
  caret=n("span","caret");
  turn.appendChild(caret);
}
function renderTurn(){
  if(!turn)return;
  md(turn,text);
  turn.__raw=text;
  if(caret)turn.appendChild(caret);
}
function paintTurn(){
  painted=false;
  if(!turn)return;
  renderTurn();
  if(findOpen())runFind(find.input.value);
  scrollEnd();
}
function appendText(chunk){
  if(!turn)startTurn();
  text+=chunk;
  if(painted)return;
  painted=true;
  requestAnimationFrame(paintTurn);
}
function attachCopy(pre){
  const code=pre.textContent,copy=n("button","gx-copy","copy");
  copy.type="button";
  copy.addEventListener("click",async event=>{
    event.stopPropagation();
    if(!(await copyText(code,"đoạn code")))return;
    copy.textContent="ok";
    copy.classList.add("is-done");
    setTimeout(()=>{copy.textContent="copy";copy.classList.remove("is-done");},1400);
  });
  pre.appendChild(copy);
}
function endTurn(){
  // `done` can race the last chunk: flush the queued paint before the turn is finalised.
  if(painted){painted=false;renderTurn();}
  if(caret&&caret.parentNode)caret.remove();
  caret=null;
  if(turn&&!turn.dataset.decorated){
    turn.dataset.decorated="1";
    for(const pre of turn.querySelectorAll("pre"))attachCopy(pre);
  }
  turn=null;
  text="";
  scrollEnd();
}

function legacyCopy(value){
  const area=n("textarea");
  area.value=value;
  area.setAttribute("readonly","");
  area.style.cssText="position:fixed;top:-1000px;opacity:0";
  D.body.appendChild(area);
  area.select();
  let ok=false;
  try{ok=D.execCommand("copy");}finally{area.remove();}
  if(!ok)throw new Error("execCommand('copy') bị từ chối");
}
async function copyText(value,label){
  const content=String(value??"");
  if(!content){toast("không có gì để copy","error");return false;}
  try{
    if(navigator.clipboard&&isSecureContext)await navigator.clipboard.writeText(content);
    else legacyCopy(content);
  }catch(error){
    try{legacyCopy(content);}catch(fallback){toast("copy thất bại: "+err(fallback),"error");return false;}
  }
  toast("đã copy "+(label||"nội dung"),"success",1700);
  return true;
}
function toast(message,kind,ttl){
  if(!toasts){
    toasts=n("div","gx-toasts");
      D.body.appendChild(toasts);
    fit();
  }
  const type=kind==="success"||kind==="error"?kind:"info";
  const life=ttl||(type==="error"?5200:type==="success"?2600:3200);
  const node=n("div","gx-toast",message),bar=n("div","gx-bar");
  node.style.borderLeftColor=bar.style.background="var(--"+(type==="success"?"accent":type==="error"?"bad":"ink-3")+")";
  bar.style.animationDuration=life+"ms";
  node.appendChild(bar);
  const dismiss=()=>node.remove();
  node.addEventListener("click",dismiss);
  toasts.appendChild(node);
  while(toasts.children.length>4)toasts.firstElementChild.remove();
  setTimeout(dismiss,life);
  return node;
}

const stack=[];
function focusables(root){
  const nodes=root.querySelectorAll("button:not([disabled]),a[href],input,[tabindex]:not([tabindex='-1'])");
  return Array.from(nodes).filter(node=>node.getClientRects().length);
}
function mount(root){
  root.classList.add("is-open");
  stack.push([root,D.activeElement]);
  const target=root.querySelector("[data-autofocus]")||focusables(root)[0];
  if(target)target.focus();
}
function unmount(root){
  const at=stack.findIndex(entry=>entry[0]===root);
  if(at<0)return;
  const pair=stack.splice(at,1)[0];
  pair[0].classList.remove("is-open");
  if(pair[1]&&pair[1].isConnected&&pair[1].focus)pair[1].focus();
}
function closeTop(){
  const top=stack[stack.length-1];
  if(!top)return false;
  unmount(top[0]);
  return true;
}
function layer(cls,label){
  const root=n("div","gx-overlay "+cls);
  root.setAttribute("role","dialog");
  root.setAttribute("aria-modal","true");
  root.setAttribute("aria-label",label);
  root.addEventListener("keydown",event=>{
    if(event.key!=="Tab")return;
    const items=focusables(root);
    if(!items.length)return;
    const first=items[0],last=items[items.length-1],active=D.activeElement;
    if(event.shiftKey&&(active===first||!root.contains(active))){event.preventDefault();last.focus();}
    else if(!event.shiftKey&&(active===last||!root.contains(active))){event.preventDefault();first.focus();}
  });
  root.addEventListener("pointerdown",event=>{if(event.target===root)unmount(root);});
  D.body.appendChild(root);
  return root;
}
function sheet(title,rows,note){
  const root=layer("gx-sheet-overlay",title),wrap=n("div","gx-sheet"),head=n("div","gx-head");
  const close=n("button","ghost","esc");
  close.type="button";
  close.addEventListener("click",()=>unmount(root));
  head.append(n("span","",title),close);
  const grid=n("div","gx-grid");
  for(const row of rows)grid.append(n("div","",row[0]),n("div","",row[1]));
  wrap.append(head,grid);
  if(note)wrap.appendChild(n("div","gx-item",note));
  root.appendChild(wrap);
  return root;
}
function openShortcuts(){
  const rows=[["ctrl ↵","gửi prompt"],["esc","dừng stream / đóng lớp phủ"],["ctrl k","bảng lệnh"],["↑ ↓ ↵","chọn và chạy lệnh"],["kéo-thả","đính kèm file text ≤256KB"]];
  for(const command of commands())if(/^(ctrl|esc|↵|↑)/.test(command[1]||""))rows.push([command[1],command[0]]);
  mount(sheet("phím tắt",rows,"esc đóng · click nền để đóng"));
}
const config={state:"unknown",data:null};
let metaInfo=null;
function openSettings(){
  // /api/config may report null model/effort, and every field is read-only here.
  const show=value=>value==null||value===""?"—":String(value);
  const info=metaInfo||{},data=config.state==="ready"&&config.data?config.data:null;
  const rows=[["provider",show(data?data.provider:info.provider)],["model",show(data?data.model:info.model)],["effort",show(data&&data.effort)],["version",show(info.version)],["cwd",show(data?data.cwd:info.cwd)]];
  if(data)rows.push(["eventLog",data.eventLog?"bật":"tắt"],["readOnly",data.readOnly?"có":"không"]);
  const note=data?"nguồn: GET /api/config + /api/meta · chỉ đọc, không sửa ở đây":config.state==="missing"?"/api/config 404 — build này chỉ có /api/meta":"/api/config không đọc được — chỉ hiển thị /api/meta";
  mount(sheet("settings · chỉ đọc",rows,note));
}

// palette: fuzzy filter on label + hint, arrows, enter, tab trap
const pal={root:null,input:null,listEl:null,all:[],shown:[],at:0};
function fuzzy(query,target){
  const needle=query.toLowerCase().replace(/\s+/g,"");
  if(!needle)return{score:0};
  const hay=target.toLowerCase();
  let cursor=0,score=0,streak=0;
  for(const ch of needle){
    const at=hay.indexOf(ch,cursor);
    if(at<0)return null;
    if(at===cursor)score+=6+ ++streak;
    else{streak=0;score+=2;}
    if(!at||/[\s\-_/:.>]/.test(hay[at-1]))score+=4;
    cursor=at+1;
  }
  return{score:score-Math.max(0,target.length-needle.length)*0.05};
}
function commands(){
  const out=[
    ["focus composer","gõ",()=>box.focus()],
    ["mở settings (chỉ đọc)","ctrl ,",openSettings],
    ["bảng phím tắt","ctrl /",openShortcuts],
    ["tìm trong transcript","ctrl f",()=>openFind()],
    ["xoá transcript","ctrl l",()=>clearTranscript()],
    [(reasoning?"ẩn":"hiện")+" panel reasoning","",()=>toggleReasoning()],
    ["copy câu trả lời cuối","",()=>copyLastAnswer()],
    ["reload dữ liệu phiên","↻",async()=>{await loadMeta(true);await loadSessions();await loadConfig();toast("đã tải lại meta + phiên","success",1800);}],
    ["copy session id gần nhất","id",()=>copyText(String(list[0]?list[0].id||"":""),"session id")],
  ];
  const prompts=Array.from(blank.querySelectorAll("[data-prompt]")).map(node=>node.dataset.prompt).filter(Boolean);
  for(const prompt of (prompts.length?prompts:PROMPTS).slice(0,3))out.push(["gửi gợi ý: "+cut(prompt,44),"gửi",()=>sendPrompt(prompt)]);
  if(list[0]&&list[0].id)out.push(["xem lại phiên "+String(list[0].id).slice(0,8),"replay",()=>startReplay(list[0].id)]);
  if(running)out.unshift(["dừng stream đang chạy","esc",()=>cancelRun("palette")]);
  return out;
}
function ensurePalette(){
  if(pal.root)return;
  const root=layer("gx-palette","bảng lệnh"),panel=n("div","gx-panel"),field=n("div","gx-field"),input=n("input","gx-input");
  input.type="text";
  input.placeholder="lệnh…";
  input.autocomplete="off";
  input.setAttribute("data-autofocus","1");
  field.appendChild(input);
  const listEl=n("div","gx-list");
  panel.append(field,listEl);
  root.appendChild(panel);
  input.addEventListener("input",()=>filterPalette(input.value));
  input.addEventListener("keydown",event=>{
    if(event.key==="ArrowDown"||event.key==="ArrowUp"){
      event.preventDefault();
      if(!pal.shown.length)return;
      pal.at=(pal.at+(event.key==="ArrowDown"?1:-1)+pal.shown.length)%pal.shown.length;
      paintSelection();
    }else if(event.key==="Enter"){
      event.preventDefault();
      runSelection();
    }
  });
  pal.root=root;
  pal.input=input;
  pal.listEl=listEl;
}
function filterPalette(query){
  const scored=[];
  for(const command of pal.all){
    const match=fuzzy(query,command[0]+" "+command[1]);
    if(match)scored.push({command,score:match.score});
  }
  scored.sort((a,b)=>b.score-a.score);
  pal.shown=scored;
  pal.at=0;
  pal.listEl.textContent="";
  if(!scored.length)pal.listEl.appendChild(n("div","gx-item","không có lệnh khớp"));
  scored.forEach((entry,index)=>{
    const item=n("button","gx-item",entry.command[0]);
    item.type="button";
    item.id="gx-cmd-"+index;
    item.setAttribute("role","option");
    item.addEventListener("pointerdown",event=>event.preventDefault());
    item.addEventListener("click",()=>{pal.at=index;runSelection();});
    pal.listEl.appendChild(item);
  });
  paintSelection();
}
function paintSelection(){
  const items=pal.listEl.querySelectorAll(".gx-item");
  items.forEach((item,index)=>item.classList.toggle("is-active",index===pal.at));
  const active=items[pal.at];
  if(active&&active.scrollIntoView)active.scrollIntoView({block:"nearest"});
}
function runSelection(){
  const entry=pal.shown[pal.at];
  if(!entry)return;
  unmount(pal.root);
  try{entry.command[2]();}catch(error){toast("lệnh lỗi: "+err(error),"error");}
}
function openPalette(){
  ensurePalette();
  pal.all=commands();
  pal.input.value="";
  filterPalette("");
  mount(pal.root);
  pal.input.focus();
}

// find in transcript: wraps only text nodes in <mark>, never re-parses markdown
const find={root:null,input:null,count:null,hits:[],at:-1};
function findOpen(){return !!(find.root&&find.root.classList.contains("is-on"));}
function ensureFind(){
  if(find.root)return;
  const root=n("div","gx-find"),input=n("input"),count=n("span");
  input.type="text";
  input.placeholder="tìm…";
  input.autocomplete="off";
  root.setAttribute("role","search");
  root.append(input,count);
  for(const action of [["↑",()=>stepFind(-1)],["↓",()=>stepFind(1)],["×",()=>closeFind()]]){
    const button=n("button","",action[0]);
    button.type="button";
    button.addEventListener("click",action[1]);
    root.appendChild(button);
  }
  input.addEventListener("input",()=>runFind(input.value));
  input.addEventListener("keydown",event=>{
    if(event.key==="Enter"){event.preventDefault();stepFind(event.shiftKey?-1:1);}
    else if(event.key==="Escape"){event.preventDefault();closeFind();}
  });
  D.body.appendChild(root);
  find.root=root;
  find.input=input;
  find.count=count;
}
function textNodes(root){
  const nodes=[],walker=D.createTreeWalker(root,NodeFilter.SHOW_TEXT,{
    acceptNode(node){
      if(!node.nodeValue||!node.nodeValue.trim())return NodeFilter.FILTER_REJECT;
      return node.parentElement&&node.parentElement.closest("button,.gx-find,.gx-toasts,.gx-jump,.gx-banner,.gx-replay,.gx-drop")?NodeFilter.FILTER_REJECT:NodeFilter.FILTER_ACCEPT;
    },
  });
  while(walker.nextNode())nodes.push(walker.currentNode);
  return nodes;
}
function clearHits(){
  for(const mark of find.hits){
    const parent=mark.parentNode;
    if(!parent)continue;
    parent.replaceChild(D.createTextNode(mark.textContent),mark);
    parent.normalize();
  }
  find.hits=[];
  find.at=-1;
}
function runFind(query){
  clearHits();
  const needle=String(query||""),hits=[];
  if(needle){
    const lower=needle.toLowerCase();
    for(const node of textNodes(T)){
      const value=node.nodeValue,hay=value.toLowerCase();
      let at=hay.indexOf(lower),cursor=0;
      if(at<0)continue;
      const frag=D.createDocumentFragment();
      while(at>=0){
        frag.appendChild(D.createTextNode(value.slice(cursor,at)));
        const mark=n("mark","gx-hit",value.slice(at,at+needle.length));
        frag.appendChild(mark);
        hits.push(mark);
        cursor=at+needle.length;
        at=hay.indexOf(lower,cursor);
      }
      frag.appendChild(D.createTextNode(value.slice(cursor)));
      node.parentNode.replaceChild(frag,node);
    }
  }
  find.hits=hits;
  find.at=hits.length?0:-1;
  paintCount(false);
}
function paintCount(smooth){
  const total=find.hits.length;
  if(find.count)find.count.textContent=total?find.at+1+"/"+total:find.input&&find.input.value?"0/0":"";
  find.hits.forEach((mark,index)=>mark.classList.toggle("is-active",index===find.at));
  const active=find.hits[find.at];
  if(active&&active.scrollIntoView)active.scrollIntoView({block:"center",behavior:smooth?"smooth":"auto"});
}
function stepFind(delta){
  const total=find.hits.length;
  if(!total)return;
  find.at=(find.at+delta+total)%total;
  paintCount(true);
}
function openFind(){
  ensureFind();
  find.root.classList.add("is-on");
  find.input.focus();
  if(find.input.value)runFind(find.input.value);
}
function closeFind(){
  if(!find.root)return;
  find.root.classList.remove("is-on");
  clearHits();
  find.count.textContent="";
}

// connection state: banner + exponential backoff, plus a slow heartbeat when healthy
let offline=false,healthTimer=0,tries=0;
function ensureBanner(){
  if(banner)return banner;
  banner=n("div","gx-banner");
  banner.setAttribute("role","alert");
  bannerMsg=n("span","","cli không phản hồi");
  bannerMsg.style.cssText="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis";
  banner.append(bannerMsg);
  D.body.appendChild(banner);
  return banner;
}
function setOffline(down,detail){
  offline=down;
  ensureBanner().classList.toggle("is-on",down);
  if(down)bannerMsg.textContent="cli không phản hồi · "+cut(detail||"không rõ lý do",72)+" · đang thử lại";
}
function scheduleHealth(){
  clearTimeout(healthTimer);
  // Backoff while broken, rare heartbeat while healthy: never a polling storm.
  healthTimer=setTimeout(()=>loadMeta(),offline?Math.min(15000,1000*2**Math.min(Math.max(tries-1,0),4)):30000);
}
async function loadMeta(manual){
  try{
    const health=await fetch("/api/health",{cache:"no-store"});
    if(!health.ok)throw new Error("HTTP "+health.status);
    const response=await fetch("/api/meta",{cache:"no-store"});
    if(!response.ok)throw new Error("HTTP "+response.status);
    const info=await response.json(),wasOffline=offline;
    metaInfo=info;
    tries=0;
    setOffline(false);
    cwd.textContent=info.cwd||"—";
    cwd.title=info.cwd||"";
    meta.textContent=(info.provider||"provider")+"\n"+(info.model||"no model")+" · v"+(info.version||"?");
    D.title="goon — "+(info.model||"no model");
    if(manual||wasOffline)toast("đã kết nối lại cli","success");
  }catch(error){
    tries++;
    setOffline(true,err(error));
    status.textContent="offline";
    status.dataset.state="idle";
    if(manual)toast("cli vẫn không phản hồi","error");
  }
  scheduleHealth();
}
async function loadConfig(){
  try{
    const response=await fetch("/api/config",{cache:"no-store"});
    if(response.status===404)config.state="missing";
    else if(!response.ok)config.state="error";
    else{config.data=await response.json();config.state="ready";}
  }catch(error){config.state="error";}
}

// session rail: click a row to replay it, the copy button keeps its id for shell use
function sessionRow(session){
  const row=n("div","session"),id=String(session.id||"");
  if(replayId&&replayId===id)row.classList.add("is-active");
  const wrap=n("div","wrap");
  wrap.append(n("span","title",cut(session.title||session.cwd||"phiên",26)),n("span","sub",id.slice(0,8)+" · "+(session.model||"?")));
  const copy=n("button","copy","id");
  copy.type="button";
  copy.addEventListener("click",async event=>{
    event.stopPropagation();
    if(!(await copyText(id,"session id")))return;
    copy.textContent="ok";
    setTimeout(()=>{copy.textContent="id";},1200);
  });
  row.addEventListener("click",()=>startReplay(id));
  row.append(wrap,copy);
  return row;
}
async function loadSessions(){
  rail.textContent="";
  try{
    const data=await (await fetch("/api/sessions",{cache:"no-store"})).json();
    list=Array.isArray(data)?data:[];
    if(!list.length)rail.appendChild(n("div","session","chưa có phiên"));
    else for(const session of list.slice(0,24))rail.appendChild(sessionRow(session));
  }catch(error){
    list=[];
    rail.appendChild(n("div","session","không đọc được phiên"));
  }
}

// replay: any endpoint shape (bare array, {envelopes}, {events}) normalises to event objects
let chip=null;
function ensureChip(){
  if(chip)return chip;
  const bar=qs(".bar");
  if(!bar)return null;
  chip=n("div","chip gx-replay");
  chip.style.cssText="display:flex;align-items:center;gap:6px;padding:2px 3px 2px 8px;color:var(--warn)";
  chip.hidden=true;
  chip.setAttribute("role","status");
  chip.appendChild(n("b","",""));
  const exit=n("button","","phiên mới");
  exit.type="button";
  exit.addEventListener("click",()=>exitReplay(true));
  chip.appendChild(exit);
  bar.insertBefore(chip,status);
  return chip;
}
function paintChip(){
  const node=ensureChip();
  if(!node)return;
  node.hidden=!replayId;
  if(replayId)node.firstElementChild.textContent="đang xem lại phiên "+replayId.slice(0,8);
}
function exitReplay(clear){
  const had=!!replayId;
  replayId=null;
  paintChip();
  if(had&&clear){
    T.replaceChildren();
    showBlank();
    clearHits();
    paintCount();
    stuck=true;
    unseen=false;
    paintJump();
    toast("đã thoát replay · phiên mới","info",1700);
  }
  loadSessions();
}
function replayEvents(payload){
  const outer=payload&&typeof payload==="object"?payload:{};
  const rows=Array.isArray(payload)?payload:outer.envelopes||outer.events||outer.items||[];
  const events=[];
  for(const envelope of rows){
    if(!envelope||typeof envelope!=="object")continue;
    const event=envelope.event&&typeof envelope.event==="object"?envelope.event:envelope;
    const type=event.type||event.kind||envelope.kind;
    if(type)events.push(event.type?event:{...event,type});
  }
  return{events,truncated:!!(outer.truncated||outer.clipped)};
}
async function startReplay(id){
  if(!id)return;
  if(running){toast("đang chạy — dừng stream trước khi xem lại","error");return;}
  let data=null;
  try{
    const response=await fetch("/api/sessions/"+encodeURIComponent(id)+"/events",{cache:"no-store"});
    if(!response.ok){
      const why=response.status===400?"id phiên không hợp lệ":response.status===404?"không thấy phiên":response.status===413?"event log quá lớn (>16MiB)":"HTTP "+response.status;
      toast("chưa xem lại được: "+why,"error");
      return;
    }
    data=await response.json();
  }catch(error){
    toast("không gọi được endpoint replay: "+err(error),"error");
    return;
  }
  const {events,truncated}=replayEvents(data);
  if(!events.length){toast("phiên này không có event để dựng lại","info");return;}
  endTurn();
  replayId=id;
  replaying=true;
  stuck=true;
  unseen=false;
  paintJump();
  T.replaceChildren();
  if(blank.isConnected)blank.remove();
  blankAway=true;
  clearHits();
  paintCount();
  paintChip();
  let cursor=0;
  const batch=()=>{
    const until=Date.now()+12;
    while(cursor<events.length&&Date.now()<until)renderEvent(events[cursor++]);
    if(cursor<events.length){setTimeout(batch,0);return;}
    endTurn();
    addRow([{text:"replay ",className:"tool"},{text:events.length+" event"+(truncated?" (đã cắt)":""),className:"arg"}]);
    replaying=false;
    scrollEnd(true);
    loadSessions();
  };
  setTimeout(batch,0);
}

const ROWS={
  usage:e=>e.total?["tokens "+e.total]:null,
  verify_start:e=>[{text:"verify ",className:"tool"},{text:(e.commands||[]).join(" · "),className:"arg"}],
  verify_result:e=>[{text:"verify ",className:"tool"},{text:e.status||"",className:e.status==="fail"?"bad":"ok"}],
  verify_rollback:e=>[{text:"rollback ",className:"warn"},e.reason||""],
  compaction:e=>["compaction #"+(e.count??0)],
  retry:e=>[{text:"retry ",className:"warn"},"attempt "+(e.attempt??0)],
  stopped:e=>[{text:"stopped ",className:"warn"},e.reason||""],
};
function renderEvent(event){
  const meta=event.meta||{};
  switch(event.type){
    case "text":
      appendText(event.text||"");
      return;
    case "reasoning":
      addPanel("thinking",event.text||"","reasoning");
      return;
    case "tool":{
      const name=event.name||"tool",label=meta.summary||meta.kind||"";
      addRow([{text:name+" ",className:"tool"},{text:event.state==="error"?label||"failed":label,className:event.state==="start"?"arg":event.state==="error"?"bad":"ok"}]);
      if(meta.diff)addPanel(name+" · diff",meta.diff);
      if(meta.outputPreview)addPanel(name+" · output",meta.outputPreview.join("\n"));
      return;
    }
    case "error":
      endTurn();
      addTurn("error",event.text||event.message||"error");
      return;
    case "done":
      if(event.text&&!text)appendText(event.text);
      endTurn();
      return;
    case "harness":
    case "task_start":
    case "jit_repaired":
    case "harness_hook":
      return;
    default:{
      const build=ROWS[event.type];
      if(build){const parts=build(event);if(parts)addRow(parts);}
    }
  }
}

async function cancelRun(source){
  if(!running){toast("không có stream đang chạy","info",1600);return;}
  try{
    const payload=await (await fetch("/api/cancel",{method:"POST"})).json();
    if(!payload.ok){
      addRow([{text:"dừng ",className:"bad"},payload.reason||"thất bại"]);
      toast(payload.reason||"không dừng được","error");
      return;
    }
    addRow([{text:"dừng ",className:"warn"},"đã gửi ("+source+")"]);
    if(abort)abort.abort();
  }catch(error){toast("dừng thất bại: "+err(error),"error");}
}
async function runPrompt(prompt){
  endTurn();
  addTurn("user",prompt);
  setRunning(true);
  abort=new AbortController();
  try{
    const response=await fetch("/api/prompt",{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({prompt}),signal:abort.signal});
    if(!(response.headers.get("content-type")||"").includes("text/event-stream")){
      const payload=await response.json().catch(()=>({}));
      // 409 means another run owns the window; 400 means an empty prompt. Both must be visible.
      const why=payload.error||(response.status===409?"đang có run khác chạy":response.status===400?"prompt rỗng":"không chạy được");
      addTurn("error",why+" (HTTP "+response.status+")");
      toast(why,"error");
      return;
    }
    const reader=response.body.getReader(),decoder=new TextDecoder();
    let buffer="";
    for(;;){
      const {value,done}=await reader.read();
      if(done)break;
      buffer+=decoder.decode(value,{stream:true});
      for(let end=buffer.indexOf("\n\n");end>=0;end=buffer.indexOf("\n\n")){
        const chunk=buffer.slice(0,end);
        buffer=buffer.slice(end+2);
        for(const line of chunk.split("\n")){
          if(!line.startsWith("data: "))continue;
          let event;
          try{event=JSON.parse(line.slice(6));}catch(error){continue;}
          if(event.type!=="exit"){renderEvent(event);continue;}
          endTurn();
          if(event.stderr)addPanel("stderr",event.stderr);
          if(event.cancelled)addRow([{text:"huỷ ",className:"warn"},"run bị dừng từ cli"]);
          if(event.code&&event.code!==0)addRow([{text:"exit ",className:"bad"},String(event.code)]);
        }
      }
    }
  }catch(error){
    endTurn();
    if(error&&error.name==="AbortError")addRow([{text:"đã dừng ",className:"warn"},"stream cục bộ đã ngắt"]);
    else{addTurn("error",err(error));toast("stream lỗi","error");}
  }finally{
    abort=null;
    endTurn();
    setRunning(false);
    loadMeta();
    loadSessions();
    loadConfig();
  }
}

function clearTranscript(){
  if(running){toast("đang chạy — dừng stream trước khi xoá","error");return;}
  endTurn();
  replayId=null;
  paintChip();
  T.replaceChildren();
  showBlank();
  clearHits();
  paintCount();
  stuck=true;
  unseen=false;
  paintJump();
  toast("đã xoá transcript","info",1600);
}
function toggleReasoning(){
  reasoning=!reasoning;
  for(const panel of T.querySelectorAll("details.panel.reasoning"))panel.hidden=!reasoning;
  toast(reasoning?"hiện panel reasoning":"ẩn panel reasoning","info",1600);
}
function copyLastAnswer(){
  const turns=T.querySelectorAll(".turn.assistant"),node=turns[turns.length-1];
  if(!node){toast("chưa có câu trả lời","error");return;}
  return copyText(node.__raw||node.textContent,"câu trả lời cuối");
}
function syncComposer(){
  counter.textContent=String(box.value.length);
  box.style.height="auto";
  box.style.height=Math.min(box.scrollHeight,200)+"px";
  fit();
}
function sendPrompt(prompt){
  if(running){toast("đang chạy — chờ stream xong","error");return;}
  if(replayId)exitReplay(false);
  box.value=prompt;
  syncComposer();
  box.focus();
  form.requestSubmit();
}

// attachments: only text ever reaches the composer, judged by content (NUL / non UTF-8)
const drop={root:null,depth:0};
async function readAttachment(file){
  if(file.size>MAX)throw new Error(file.name+": >256KB");
  if(!file.size)throw new Error(file.name+": file rỗng");
  const bytes=new Uint8Array(await file.arrayBuffer());
  if(bytes.subarray(0,Math.min(bytes.length,8192)).includes(0))throw new Error(file.name+": nhị phân");
  try{return new TextDecoder("utf-8",{fatal:true}).decode(bytes);}
  catch(error){throw new Error(file.name+": không phải UTF-8");}
}
function attachmentBlock(name,body){
  let longest=0;
  for(const run of body.match(/`+/g)||[])longest=Math.max(longest,run.length);
  const fence="`".repeat(Math.min(6,Math.max(3,longest+1)));
  return fence+"file:"+name+"\n"+body.replace(/\s+$/,"")+"\n"+fence+"\n";
}
function insertIntoComposer(content){
  const value=box.value;
  const start=typeof box.selectionStart==="number"?box.selectionStart:value.length;
  const end=typeof box.selectionEnd==="number"?box.selectionEnd:start;
  const before=value.slice(0,start),gap=before&&!/\n$/.test(before)?"\n":"";
  box.value=before+gap+content+value.slice(end);
  const at=(before+gap+content).length;
  box.setSelectionRange(at,at);
  syncComposer();
  box.focus();
}
function ensureDrop(){
  if(drop.root)return;
  drop.root=n("div");
  drop.root.style.cssText="position:fixed;inset:0;z-index:65;display:none;place-items:center;pointer-events:none;border:2px dashed var(--accent)";
  const label=n("span","chip","thả file text để đính kèm (≤256KB)");
  label.style.color="var(--accent)";
  drop.root.appendChild(label);
  D.body.appendChild(drop.root);
}
async function handleDrop(files){
  let added=0;
  for(const file of Array.from(files||[])){
    try{
      insertIntoComposer(attachmentBlock(file.name,await readAttachment(file)));
      added++;
    }catch(error){toast(err(error),"error");}
  }
  if(added)toast("đã đính kèm "+added+" file","success",2000);
}
for(const type of ["dragenter","dragover"]){
  window.addEventListener(type,event=>{
    if(!event.dataTransfer||!Array.from(event.dataTransfer.types||[]).includes("Files"))return;
    event.preventDefault();
    if(type==="dragenter")drop.depth++;
    ensureDrop();
    drop.root.style.display="grid";
  },{passive:false});
}
window.addEventListener("dragleave",event=>{
  if(!event.dataTransfer)return;
  drop.depth=Math.max(0,drop.depth-1);
  if(!drop.depth&&drop.root)drop.root.style.display="none";
});
window.addEventListener("drop",event=>{
  event.preventDefault();
  drop.depth=0;
  if(drop.root)drop.root.style.display="none";
  if(event.dataTransfer&&event.dataTransfer.files.length)handleDrop(event.dataTransfer.files);
});

// keyboard: ctrl+k palette, ctrl+f find, ctrl+l clear, ctrl+/ and ctrl+, sheets, esc backs out
window.addEventListener("keydown",event=>{
  const mod=event.ctrlKey||event.metaKey,key=event.key,lower=String(key).toLowerCase();
  if(mod&&!event.altKey){
    if(lower==="k"){
      event.preventDefault();
      if(pal.root&&stack.some(entry=>entry[0]===pal.root))unmount(pal.root);
      else openPalette();
      return;
    }
    if(lower==="l"){event.preventDefault();clearTranscript();return;}
    if(key==="/"||key==="?"||key===","){
      event.preventDefault();
      if(key===",")openSettings();
      else openShortcuts();
      return;
    }
    if(lower==="f"){
      event.preventDefault();
      if(findOpen())closeFind();
      else openFind();
      return;
    }
  }
  if(key!=="Escape")return;
  if(stack.length){event.preventDefault();closeTop();return;}
  if(findOpen()){event.preventDefault();closeFind();return;}
  if(running){event.preventDefault();cancelRun("esc");}
},{capture:true});

buildKinetic();
setTimeout(endIntro,1900);
const hint=qs(".composer-hint");
if(hint)hint.appendChild(D.createTextNode(" · ctrl K lệnh · ctrl F tìm"));
form.addEventListener("submit",event=>{
  event.preventDefault();
  const prompt=box.value.trim();
  if(!prompt||running)return;
  if(replayId)exitReplay(false);
  box.value="";
  box.style.height="auto";
  counter.textContent="0";
  fit();
  runPrompt(prompt);
});
stop.addEventListener("click",()=>cancelRun("nút dừng"));
box.addEventListener("input",syncComposer);
box.addEventListener("keydown",event=>{
  if(event.key==="Enter"&&(event.ctrlKey||event.metaKey)){event.preventDefault();form.requestSubmit();}
});
for(const el of D.querySelectorAll(".chip"))el.addEventListener("click",()=>{box.value=el.dataset.prompt||"";syncComposer();box.focus();});
window.addEventListener("resize",fit);
if(typeof ResizeObserver==="function")new ResizeObserver(fit).observe(form);
jump=n("button","chip gx-jump","↓ xuống cuối");
jump.style.cssText="position:fixed;right:22px;bottom:var(--gx-chrome,104px);z-index:45;display:none;font-size:10.5px";
jump.type="button";
jump.addEventListener("click",()=>{stuck=true;unseen=false;scrollEnd(true);});
D.body.appendChild(jump);
ensureChip();
paintChip();
fit();
loadMeta();
loadConfig();
loadSessions();
