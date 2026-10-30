import {expect,test} from "@playwright/test";
// Obviously fake: the shape of a real atomik-kb token, never a real one.
const FAKE_TOKEN="kbm_TEST_FAKE_TOKEN_aaaaaaaaaaaaaaaaaaaaaaaaaaa";
test.beforeEach(async({page})=>{await page.addInitScript(()=>{
  const rec=(id:string)=>({recording_id:id,started_at:1,ended_at:2,duration_ms:1,status:"recorded",tracks:[],error:null,transcription_status:"",appflowy_status:""});
  const f:any={
    calls:[],
    settings:{auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",diarization_enabled:true,audio_sources:[],summary_enabled:true,ollama_url:"http://127.0.0.1:11434",summary_model:"qwen3:8b",appflowy_enabled:false,appflowy_auto_export:false,appflowy_base_url:"https://beta.appflowy.cloud",appflowy_use_native_block:true,appflowy_workspace_id:null,appflowy_parent_view_id:null,atomik_kb_enabled:false,atomik_kb_auto_export:false,atomik_kb_port:47811,atomik_kb_space_name:"My Daily Meeting"},
    appflowy:{enabled:false,auto_export:false,connected:false,email:null,base_url:"https://beta.appflowy.cloud",jobs:[],inhibited:false,error:null},
    kb:{enabled:false,auto_export:false,has_token:false,port:47811,space_name:"My Daily Meeting",jobs:[],inhibited:false,error:null},
    pages:[],
    tokenError:null as string|null,
    testResult:{ok:true,message:'Connected. The space "My Daily Meeting" was found and is writable.',space_found:true,writable:true},
  };
  Object.assign(window,{__kb:f,__TAURI_INTERNALS__:{transformCallback:()=>1,invoke:async(c:string,a:any)=>{
    f.calls.push([c,a]);
    if(c==="plugin:event|listen")return 1;
    if(c==="get_settings")return f.settings;
    if(c==="save_settings"){f.settings={...f.settings,...a.settings};return f.settings}
    if(c==="get_app_state")return{status:"idle",recording_supported:true,detection_supported:false,appflowy_connected:false,recordings_folder:"x",tray_available:true,tray_error:null};
    if(c==="get_recording_state")return{status:"idle",active:null,recent:[rec("a")],error:null};
    if(c==="get_detection_state")return{supported:false,enabled:false,phase:"unsupported",detail:"",provider:null,error:null,seconds_remaining:null,automation_inhibited:false};
    if(c==="get_transcription_state")return{inhibited:false,model:{key:"base",available:false},jobs:[{recording_id:"a",status:"completed",progress:100,error:null,markdown_path:null}],error:null,backend:"cpu (AVX2)"};
    if(c==="get_diarization_state")return{enabled:true,inhibited:false,models:[],jobs:[],error:null};
    if(c==="get_summary_state")return{enabled:true,inhibited:false,jobs:[],error:null};
    if(c==="get_compression_state")return{enabled:true,inhibited:false,jobs:[],error:null};
    if(c==="audio_storage_summary")return{total_bytes:0,total_original_bytes:0,recordings:[]};
    if(c==="get_audio_devices")return{backend:"x",devices:[],errors:[]};
    if(c==="get_appflowy_state")return f.appflowy;
    if(c==="appflowy_list_pages")return [];
    if(c==="get_atomik_kb_state")return f.kb;
    if(c==="atomik_kb_list_pages")return f.pages;
    if(c==="atomik_kb_save_token"){if(f.tokenError)throw Error(f.tokenError);f.savedToken=a.token;f.kb={...f.kb,has_token:true};return null}
    if(c==="atomik_kb_clear_token"){f.kb={...f.kb,has_token:false};return null}
    if(c==="atomik_kb_test_connection")return f.testResult;
    if(c==="atomik_kb_export_now"){f.kb={...f.kb,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:1}]};return null}
    if(c==="atomik_kb_export_again"){if(!a.confirm)throw Error("Exporting again requires confirmation.");f.kb={...f.kb,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:2}]};return null}
    if(c==="atomik_kb_retry"){f.kb={...f.kb,jobs:[{recording_id:a.recordingId,status:"queued",error:null,export_index:1}]};return null}
    throw Error(c)
  }}})
});await page.goto("/")});

const setKb=(page:any,patch:Record<string,unknown>)=>page.evaluate((p:any)=>{const f=(window as any).__kb;f.kb={...f.kb,...p}},patch);
const callsOf=(page:any,name:string)=>page.evaluate((n:string)=>(window as any).__kb.calls.filter((x:any)=>x[0]===n),name);

test("atomik kb export is off by default, with its documented port and space name",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator('[name="atomik_kb_enabled"]')).not.toBeChecked();
  await expect(page.locator('[name="atomik_kb_auto_export"]')).not.toBeChecked();
  await expect(page.locator('[name="atomik_kb_auto_export"]')).toBeDisabled();
  await expect(page.locator('[name="atomik_kb_port"]')).toHaveValue("47811");
  await expect(page.locator('[name="atomik_kb_space_name"]')).toHaveValue("My Daily Meeting");
  await expect(page.locator("#atomik-kb-token-status")).toContainText("No token saved");
});

test("auto export is a separate opt-in that only unlocks once export is enabled",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="atomik_kb_enabled"]').check();
  await expect(page.locator('[name="atomik_kb_auto_export"]')).toBeEnabled();
  await page.locator('[name="atomik_kb_enabled"]').uncheck();
  await expect(page.locator('[name="atomik_kb_auto_export"]')).toBeDisabled();
});

test("enabling export, changing port and space name, and saving persists them",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="atomik_kb_enabled"]').check();
  await page.locator('[name="atomik_kb_auto_export"]').check();
  await page.locator('[name="atomik_kb_port"]').fill("48000");
  await page.locator('[name="atomik_kb_space_name"]').fill("Réunions 2026");
  await page.getByRole("button",{name:"Save preferences"}).click();
  const saved=await page.evaluate(()=> (window as any).__kb.calls.filter((x:any)=>x[0]==="save_settings").at(-1)[1].settings);
  expect(saved.atomik_kb_enabled).toBe(true);
  expect(saved.atomik_kb_auto_export).toBe(true);
  expect(saved.atomik_kb_port).toBe(48000);
  expect(saved.atomik_kb_space_name).toBe("Réunions 2026");
});

test("saving other preferences keeps the atomik kb settings (they are part of every save)",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.getByRole("button",{name:"Save preferences"}).click();
  const saved=await page.evaluate(()=> (window as any).__kb.calls.filter((x:any)=>x[0]==="save_settings").at(-1)[1].settings);
  expect(saved).toMatchObject({atomik_kb_enabled:false,atomik_kb_auto_export:false,atomik_kb_port:47811,atomik_kb_space_name:"My Daily Meeting"});
});

test("the token field is a password field and the token goes only to the save-token command",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  const field=page.locator("#atomik-kb-token");
  await expect(field).toHaveAttribute("type","password");
  await field.fill(FAKE_TOKEN);
  await page.getByRole("button",{name:"Save token"}).click();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("Token saved in the OS credential store");
  expect(await callsOf(page,"atomik_kb_save_token")).toEqual([["atomik_kb_save_token",{token:FAKE_TOKEN}]]);
  await expect(field).toHaveValue("");
  // The token is nowhere in the page afterwards, and never in a settings save.
  expect(await page.evaluate((t:string)=>document.body.innerHTML.includes(t)||document.body.innerText.includes(t),FAKE_TOKEN)).toBe(false);
  await page.getByRole("button",{name:"Save preferences"}).click();
  expect(await page.evaluate((t:string)=>JSON.stringify((window as any).__kb.calls.filter((x:any)=>x[0]==="save_settings"))?.includes(t),FAKE_TOKEN)).toBe(false);
  expect(await page.evaluate((t:string)=>JSON.stringify(localStorage)+JSON.stringify(sessionStorage),FAKE_TOKEN)).not.toContain(FAKE_TOKEN);
  await expect(page.locator("#atomik-kb-token-status")).not.toContainText(FAKE_TOKEN);
});

test("an empty token is refused locally and a rejected token shows the error without echoing it",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.getByRole("button",{name:"Save token"}).click();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("Paste the token first");
  expect(await callsOf(page,"atomik_kb_save_token")).toHaveLength(0);
  await page.evaluate(()=>{(window as any).__kb.tokenError="That does not look like an atomik-kb token (expected kbm_ followed by 43 characters)."});
  await page.locator("#atomik-kb-token").fill("kbm_short");
  await page.getByRole("button",{name:"Save token"}).click();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("Could not save token:");
  await expect(page.locator("#atomik-kb-token-status")).toContainText("does not look like an atomik-kb token");
  await expect(page.locator("#atomik-kb-token-status")).not.toContainText("kbm_short");
});

test("clear token removes the saved token",async({page})=>{
  await setKb(page,{has_token:true});
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("A token is saved",{timeout:5000});
  await page.getByRole("button",{name:"Clear token"}).click();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("Token removed");
  expect(await callsOf(page,"atomik_kb_clear_token")).toHaveLength(1);
  await expect(page.getByRole("button",{name:"Clear token"})).toBeDisabled();
});

test("test connection reports the result and tests the unsaved form values",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="atomik_kb_port"]').fill("48123");
  await page.locator('[name="atomik_kb_space_name"]').fill("Elsewhere");
  await page.locator("#atomik-kb-test").click();
  await expect(page.locator("#atomik-kb-test-result")).toContainText("found and is writable");
  expect(await callsOf(page,"atomik_kb_test_connection")).toEqual([["atomik_kb_test_connection",{port:48123,spaceName:"Elsewhere"}]]);
});

test("a failed connection test shows the precise problem as plain text",async({page})=>{
  await page.evaluate(()=>{(window as any).__kb.testResult={ok:false,message:'The space "<img src=x onerror=alert(1)>" is read-only for this client. In atomik-kb: Settings → MCP, grant this client write access to it.',space_found:true,writable:false}});
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#atomik-kb-test").click();
  await expect(page.locator("#atomik-kb-test-result")).toContainText("read-only");
  await expect(page.locator("#atomik-kb-test-result")).toContainText("<img src=x onerror=alert(1)>");
  await expect(page.locator("img[src=x]")).toHaveCount(0);
});

test("a meeting offers Atomik KB setup next to AppFlowy's when export is off, opening Settings",async({page})=>{
  await expect(page.getByRole("button",{name:"Set up AppFlowy export"})).toBeVisible();
  const setup=page.getByRole("button",{name:"Set up Atomik KB export"});
  await expect(setup).toBeVisible();
  await setup.click();
  await expect(page.locator("#atomik-kb-settings-section")).toBeVisible();
  await expect(page.locator('[name="atomik_kb_enabled"]')).toBeVisible();
});

test("enabled without a token offers adding the token instead of a dead export button",async({page})=>{
  await setKb(page,{enabled:true,has_token:false});
  await expect(page.getByRole("button",{name:"Add Atomik KB token"})).toBeVisible({timeout:5000});
  await expect(page.getByRole("button",{name:"Export to Atomik KB",exact:true})).toHaveCount(0);
});

test("per-meeting export button appears once enabled with a token, and calls export_now",async({page})=>{
  await setKb(page,{enabled:true,has_token:true});
  await page.getByRole("button",{name:"Export to Atomik KB",exact:true}).click({timeout:5000});
  expect(await callsOf(page,"atomik_kb_export_now")).toEqual([["atomik_kb_export_now",{recordingId:"a"}]]);
  await expect(page.getByRole("button",{name:"Export to Atomik KB again"})).toBeVisible({timeout:5000});
  await expect(page.locator("#recent-recordings")).toContainText("Exported to Atomik KB");
});

test("the AppFlowy and Atomik KB buttons are independent siblings",async({page})=>{
  await page.evaluate(()=>{const f=(window as any).__kb;f.appflowy={...f.appflowy,enabled:true,connected:true,email:"u@e.com"};f.kb={...f.kb,enabled:true,has_token:true}});
  await expect(page.getByRole("button",{name:"Export to AppFlowy",exact:true})).toBeVisible({timeout:5000});
  await expect(page.getByRole("button",{name:"Export to Atomik KB",exact:true})).toBeVisible();
  await page.getByRole("button",{name:"Export to Atomik KB",exact:true}).click();
  expect(await callsOf(page,"appflowy_export_now")).toHaveLength(0);
});

test("a completed export offers export-again behind a confirmation, and declining does nothing",async({page})=>{
  await setKb(page,{enabled:true,has_token:true,jobs:[{recording_id:"a",status:"completed",error:null,export_index:1}]});
  await expect(page.getByRole("button",{name:"Export to Atomik KB again"})).toBeVisible({timeout:5000});
  page.once("dialog",(dialog)=>dialog.dismiss());
  await page.getByRole("button",{name:"Export to Atomik KB again"}).click();
  expect(await callsOf(page,"atomik_kb_export_again")).toHaveLength(0);
  page.once("dialog",(dialog)=>dialog.accept());
  await page.getByRole("button",{name:"Export to Atomik KB again"}).click();
  expect(await callsOf(page,"atomik_kb_export_again")).toEqual([["atomik_kb_export_again",{recordingId:"a",confirm:true}]]);
  await expect(page.locator("#recent-recordings")).toContainText("Exported to Atomik KB (2)",{timeout:5000});
});

test("a failed export shows the error and a retry button",async({page})=>{
  await setKb(page,{enabled:true,has_token:true,jobs:[{recording_id:"a",status:"failed",error:'No space named "My Daily Meeting" is shared with this client',export_index:1}]});
  await expect(page.locator("#recent-recordings")).toContainText("No space named",{timeout:5000});
  await page.getByRole("button",{name:"Retry Atomik KB export"}).click();
  expect(await callsOf(page,"atomik_kb_retry")).toEqual([["atomik_kb_retry",{recordingId:"a"}]]);
});

test("an unreachable atomik-kb shows as queued with its message, not as a failure",async({page})=>{
  await setKb(page,{enabled:true,has_token:true,jobs:[{recording_id:"a",status:"queued",error:"atomik-kb is not reachable. Open it and unlock it.",export_index:1}]});
  await expect(page.locator("#recent-recordings")).toContainText("Atomik KB export queued",{timeout:5000});
  await expect(page.locator("#recent-recordings")).toContainText("atomik-kb is not reachable");
  await expect(page.locator("#recent-recordings")).not.toContainText("Atomik KB export failed");
});

test("export errors are rendered as text, not executed",async({page})=>{
  await setKb(page,{enabled:true,has_token:true,jobs:[{recording_id:"a",status:"failed",error:"<img src=x onerror=alert(1)>",export_index:1}]});
  await expect(page.locator("#recent-recordings")).toContainText("<img src=x onerror=alert(1)>",{timeout:5000});
  await expect(page.locator("img[src=x]")).toHaveCount(0);
});

test("nothing about Atomik KB is shown on meeting rows while the integration is off, except the setup button",async({page})=>{
  await page.waitForTimeout(2200);
  await expect(page.locator("#recent-recordings")).not.toContainText("Exported to Atomik KB");
  for(const text of ["Not exported to Atomik KB","Atomik KB export queued","Atomik KB export failed","Add Atomik KB token"])await expect(page.locator("#recent-recordings")).not.toContainText(text);
  await expect(page.locator("[data-atomik-kb-export],[data-atomik-kb-retry],[data-atomik-kb-export-again]")).toHaveCount(0);
});

test("meetings without a finished transcript show no Atomik KB action",async({page})=>{
  await setKb(page,{enabled:true,has_token:true});
  await page.evaluate(()=>{const t=(window as any).__TAURI_INTERNALS__;const orig=t.invoke;t.invoke=async(c:string,a:any)=>c==="get_transcription_state"?{inhibited:false,model:{key:"base",available:false},jobs:[{recording_id:"a",status:"transcribing",progress:40,error:null,markdown_path:null}],error:null,backend:"cpu (AVX2)"}:orig(c,a)});
  await page.waitForTimeout(2500);
  await expect(page.locator("[data-atomik-kb-export],[data-atomik-kb-setup]")).toHaveCount(0);
});

test("exported pages list renders escaped names and is read-only",async({page})=>{
  await page.evaluate(()=>{(window as any).__kb.pages=[{key:"root:s1",kind:"root",name:"<img src=x onerror=alert(1)>",status:"created",page_id:"p1"},{key:"date:p1:2026-09-26",kind:"date",name:"2026-09-26",status:"created",page_id:"p2"}]});
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#atomik-kb-exported-pages summary").click({timeout:5000});
  await expect(page.locator("#atomik-kb-exported-pages")).toContainText("<img src=x onerror=alert(1)>");
  await expect(page.locator("#atomik-kb-exported-pages img")).toHaveCount(0);
  await expect(page.locator("#atomik-kb-exported-pages li")).toHaveCount(2);
  await expect(page.locator("#atomik-kb-exported-pages button")).toHaveCount(0);
});

test("unsaved Atomik KB edits, including a half-typed token, survive background polling",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="atomik_kb_enabled"]').check();
  await page.locator('[name="atomik_kb_auto_export"]').check();
  await page.locator('[name="atomik_kb_port"]').fill("48555");
  await page.locator('[name="atomik_kb_space_name"]').fill("Réunions");
  await page.locator("#atomik-kb-token").fill("kbm_half");
  await page.locator("#atomik-kb-token").focus();
  // A state change that forces a re-render of the section mid-edit.
  await setKb(page,{has_token:true});
  await page.waitForTimeout(5000);
  await expect(page.locator('[name="atomik_kb_enabled"]')).toBeChecked();
  await expect(page.locator('[name="atomik_kb_auto_export"]')).toBeChecked();
  await expect(page.locator('[name="atomik_kb_port"]')).toHaveValue("48555");
  await expect(page.locator('[name="atomik_kb_space_name"]')).toHaveValue("Réunions");
  await expect(page.locator("#atomik-kb-token")).toHaveValue("kbm_half");
  await expect(page.locator("#atomik-kb-token")).toBeFocused();
  await expect(page.locator("#atomik-kb-token-status")).toContainText("A token is saved");
});
