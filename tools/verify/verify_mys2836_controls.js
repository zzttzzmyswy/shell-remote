const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const {chromium} = require('playwright');
// Run with NODE_PATH pointing to Playwright; CHROME optionally selects its executable.
// Backend and stream timing are stubbed; real decoder coverage lives in verify_mys2836_runtime.js.
const root = path.resolve(process.env.WEB_ROOT || path.join(__dirname, '../../web'));
const out = path.resolve(process.env.OUT || 'artifacts/desktop-controls');
fs.mkdirSync(out, {recursive:true});
const stub = `
window.__events={};window.__sent=[];window.__connects=0;
window.__emit=(type,msg)=>{for(const fn of window.__events[type]||[])fn(msg||{});};
window.shellRemote={on(type,fn){(window.__events[type] ||= []).push(fn);},
send(type,payload){window.__sent.push({type,payload});},sendAsync(){return Promise.resolve({});},
getPermission(){return 'rw';},getUserId(){return 'reviewer';},reconnect(){}};
`;
const desktopStub = `
window.DesktopView=class {
constructor(){window.__desktop=this;this._codecBlacklisted=[];}
connect(){window.__connects++;if(this.onProgress)this.onProgress('relay','正在通过中继（Relay）拉流…');}
disconnect(){if(this.onProgress)this.onProgress('stopped','');}
skipP2P(){if(this.onProgress)this.onProgress('relay','正在通过中继（Relay）拉流…');}
};`;
const server=http.createServer((req,res)=>{
  const url=new URL(req.url,'http://localhost').pathname;
  if(url==='/sse.js'||url==='/desktop.js'){
    res.writeHead(200,{'Content-Type':'text/javascript'});res.end(url==='/sse.js'?stub:desktopStub);return;
  }
  const file=path.resolve(root,'.'+(url==='/'?'/session.html':url));
  if(!file.startsWith(root+path.sep)||!fs.existsSync(file)||!fs.statSync(file).isFile()){res.writeHead(404);res.end();return;}
  const mime={'.html':'text/html','.js':'text/javascript','.css':'text/css','.woff2':'font/woff2'}[path.extname(file)]||'application/octet-stream';
  res.writeHead(200,{'Content-Type':mime});fs.createReadStream(file).pipe(res);
});
(async()=>{
  await new Promise(r=>server.listen(0,'127.0.0.1',r));
  const base='http://127.0.0.1:'+server.address().port;
  const browser=await chromium.launch({...(process.env.CHROME ? {executablePath:process.env.CHROME} : {}),headless:true,args:['--no-sandbox']});
  try {
    for(const viewport of [{width:1440,height:900},{width:390,height:844}])for(const theme of ['light','dark']){
      const page=await browser.newPage({viewport});
      const errors=[];page.on('pageerror',e=>errors.push(e.message));
      await page.addInitScript(theme=>{sessionStorage.setItem('shell-remote-token','visual-review-test-only');localStorage.setItem('sr-theme',theme);},theme);
      await page.goto(base+'/session.html');
      await page.evaluate(()=>{
        __emit('connected',{session_id:'mys2836-review-session-0123456789-long-session-id'});
        __emit('desktop:capabilities',{payload:{available:true,codecs:['av1','h264'],running:false}});
        __emit('session:tab_list',{payload:{tabs:[{tab_id:'t1',title:'终端'}],active_tab_id:'t1'}});
      });
      await page.locator('#toggle-desktop-btn').click();
      await page.screenshot({path:path.join(out,`${viewport.width}-${theme}-starting.png`)});
      if(process.env.BEFORE!=='1') {
        assert.equal(await page.locator('#toggle-desktop-btn').isEnabled(),true);
        assert.equal(await page.locator('#desktop-ctrl-bar').isVisible(),true);
        await page.locator('#ctrl-advanced-toggle').click();
        await page.locator('#desktop-codec-select').selectOption('h264');
        assert.ok(await page.evaluate(()=>__sent.some(x=>x.type==='desktop:codec'&&x.payload.codec==='h264')));
        await page.locator('#toggle-desktop-btn').click();
        await page.evaluate(()=>{
          __emit('desktop:started',{payload:{codec:'av1'}});
          __emit('desktop:state',{payload:{running:true,codec:'av1'}});
          __emit('desktop:capabilities',{payload:{available:true,running:true,codecs:['av1','h264']}});
        });
        assert.equal(await page.locator('#desktop-container').isVisible(),false,'取消后的迟到 started 不能抢回桌面');
        assert.equal(await page.locator('#connect-modal').isVisible(),false);
        await page.locator('#toggle-desktop-btn').click();
      }
      await page.evaluate(()=>__emit('desktop:started',{payload:{codec:'av1'}}));
      await page.waitForTimeout(220);
      await page.locator('#toast').waitFor({state:'hidden'});
      await page.screenshot({path:path.join(out,`${viewport.width}-${theme}-relay.png`)});
      if(process.env.BEFORE!=='1') {
        const handle=page.locator('#desktop-controls-move');
        const before=await handle.boundingBox();
        await page.mouse.move(before.x+before.width/2,before.y+before.height/2);
        await page.mouse.down();await page.mouse.move(before.x+50,before.y+260,{steps:8});await page.mouse.up();
        const bar=page.locator('#desktop-ctrl-bar');
        const moved=await bar.boundingBox();assert.ok(moved.y>before.y+100,'拖动应移动浮窗');
        await handle.focus();await page.keyboard.press('Home');
        assert.ok((await bar.boundingBox()).y<moved.y,'Home 复位');
        await handle.press('ArrowDown');
        await page.setViewportSize({width:320,height:640});
        await page.waitForTimeout(100);
        const b=await bar.boundingBox();assert.ok(b.x>=0&&b.x+b.width<=321&&b.y+b.height<=641,'缩屏后浮窗仍在视口');
        await page.screenshot({path:path.join(out,`${viewport.width}-${theme}-drag-clamped.png`)});
        await page.locator('#toggle-desktop-btn').click();
        assert.equal(await page.locator('#connect-modal').isVisible(),false);
      }
      assert.deepEqual(errors,[]);
      console.log('PASS',viewport.width,theme);
      await page.close();
    }
  }finally{await browser.close();await new Promise(r=>server.close(r));}
})().catch(e=>{console.error(e);server.close();process.exitCode=1;});
