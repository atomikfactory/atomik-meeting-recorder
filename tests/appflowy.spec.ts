import {expect,test} from "@playwright/test";
test.beforeEach(async({page})=>{await page.addInitScript(()=>{
  const rec=(id:string)=>({recording_id:id,started_at:1,ended_at:2,duration_ms:1,status:"recorded",tracks:[],error:null,transcription_status:"",appflowy_status:""});
  const f:any={
    calls:[],
    settings:{auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",diarization_enabled:true,audio_sources:[],summary_enabled:true,ollama_url:"http://127.0.0.1:11434",summary_model:"qwen3:8b",appflowy_enabled:false,appflowy_auto_export:false,appflowy_base_url:"https://beta.appflowy.cloud",appflowy_use_native_block:true,appflowy_workspace_id:null,appflowy_parent_view_id:null},
    appflowy:{enabled:false,auto_export:false,connected:false,email:null,base_url:"https://beta.appflowy.cloud",jobs:[],inhibited:false,error:null},
    workspaces:[{workspace_id:"ws1",workspace_name:"Test Workspace"}],
    destinations:[{view_id:"space-1",name:"General",depth:0},{view_id:"page-1",name:"Roadmap",depth:1}],
    pages:[],
    otpOk:true,
    openedUrls:[] as string[],
  };
  Object.assign(window,{__af:f,__TAURI_INTERNALS__:{transformCallback:()=>1,invoke:async(c:string,a:any)=>{
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
    if(c==="appflowy_list_workspaces")return f.workspaces;
    if(c==="appflowy_list_destinations")return f.destinations;
    if(c==="appflowy_list_pages")return f.pages;
    if(c==="open_appflowy_url"){f.openedUrls.push(a.url);return null}
    if(c==="appflowy_request_otp"){if(!f.otpOk)throw Error("AppFlowy Cloud is not reachable right now.");f.otpEmail=a.email;return null}
    if(c==="appflowy_verify_otp"){if(a.code!=="123456")throw Error("invalid code");f.appflowy={...f.appflowy,connected:true,email:a.email};return null}
    if(c==="appflowy_sign_out"){f.appflowy={...f.appflowy,connected:false,email:null};return null}
    if(c==="appflowy_export_now"){f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:1,page_url:"https://appflowy.com/app/ws1/v1"}]};return null}
    if(c==="appflowy_export_again"){if(!a.confirm)throw Error("Exporting again requires confirmation.");f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:2,page_url:"https://appflowy.com/app/ws1/v2"}]};return null}
    if(c==="appflowy_retry"){f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"queued",error:null,export_index:1,page_url:null}]};return null}
    throw Error(c)
  }}})
});await page.goto("/")});

test("appflowy export is off and not signed in by default",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Not signed in");
  await expect(page.locator('[name="appflowy_enabled"]')).not.toBeChecked();
});

test("enabling export and saving persists the setting",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="appflowy_enabled"]').check();
  await page.getByRole("button",{name:"Save preferences"}).click();
  const saved=await page.evaluate(()=> (window as any).__af.calls.filter((x:any)=>x[0]==="save_settings").at(-1)[1].settings);
  expect(saved.appflowy_enabled).toBe(true);
});

test("sign-in flow requests and verifies a one-time code",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#appflowy-email").fill("user@example.com");
  await page.getByRole("button",{name:"Send sign-in code"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="appflowy_request_otp"))).toEqual(["appflowy_request_otp",{email:"user@example.com"}]);
  await expect(page.locator("#appflowy-code")).toBeVisible();
  await page.locator("#appflowy-code").fill("123456");
  await page.getByRole("button",{name:"Verify code"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Signed in as user@example.com");
});

test("a wrong code shows an error and does not claim success",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#appflowy-email").fill("user@example.com");
  await page.getByRole("button",{name:"Send sign-in code"}).click();
  await page.locator("#appflowy-code").fill("000000");
  await page.getByRole("button",{name:"Verify code"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Could not verify code");
});

test("signed-in state shows a workspace picker and sign-out",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,connected:true,email:"user@example.com"}});
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator('select[name="appflowy_workspace_id"] option')).toHaveCount(1);
  await page.getByRole("button",{name:"Sign out"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="appflowy_sign_out"))).toBeTruthy();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Not signed in");
});

test("per-meeting export button appears once enabled and signed in, and calls export_now",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:true,email:"user@example.com"}});
  await page.waitForTimeout(2100);
  await page.getByRole("button",{name:"Export to AppFlowy"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="appflowy_export_now"))).toEqual(["appflowy_export_now",{recordingId:"a"}]);
});

test("a completed export offers export-again behind a confirmation",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:true,email:"user@example.com",jobs:[{recording_id:"a",status:"completed",error:null,export_index:1}]}});
  await page.waitForTimeout(2100);
  page.once("dialog",(dialog)=>dialog.accept());
  await page.getByRole("button",{name:"Export to AppFlowy again"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="appflowy_export_again"))).toEqual(["appflowy_export_again",{recordingId:"a",confirm:true}]);
});

test("a failed export shows the server error and a retry button",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:true,email:"user@example.com",jobs:[{recording_id:"a",status:"failed",error:"AppFlowy rejected the request (HTTP 422): bad name",export_index:1}]}});
  await page.waitForTimeout(2100);
  await expect(page.locator("#recent-recordings")).toContainText("AppFlowy rejected the request");
  await page.getByRole("button",{name:"Retry AppFlowy export"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="appflowy_retry"))).toEqual(["appflowy_retry",{recordingId:"a"}]);
});

test("export errors are rendered as text, not executed",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:true,jobs:[{recording_id:"a",status:"failed",error:"<img src=x onerror=alert(1)>",export_index:1}]}});
  await page.waitForTimeout(2100);
  await expect(page.locator("img[src=x]")).toHaveCount(0);
  await expect(page.locator("#recent-recordings")).toContainText("<img src=x onerror=alert(1)>");
});

test("unsaved AppFlowy edits survive background status polling",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="appflowy_enabled"]').check();
  await expect(page.locator('[name="appflowy_auto_export"]')).toBeEnabled();
  await page.locator('[name="appflowy_auto_export"]').check();
  await page.locator('[name="appflowy_use_native_block"]').uncheck();
  await page.locator('[name="appflowy_base_url"]').fill("https://flowy.example.com");
  await page.locator("#appflowy-email").fill("user@example.com");
  await page.locator("#appflowy-email").focus();
  await page.waitForTimeout(5000);
  await expect(page.locator('[name="appflowy_enabled"]')).toBeChecked();
  await expect(page.locator('[name="appflowy_auto_export"]')).toBeChecked();
  await expect(page.locator('[name="appflowy_use_native_block"]')).not.toBeChecked();
  await expect(page.locator('[name="appflowy_base_url"]')).toHaveValue("https://flowy.example.com");
  await expect(page.locator("#appflowy-email")).toHaveValue("user@example.com");
  await expect(page.locator("#appflowy-email")).toBeFocused();
});

test("typed sign-in code survives polling after a code is sent",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#appflowy-email").fill("user@example.com");
  await page.getByRole("button",{name:"Send sign-in code"}).click();
  await page.locator("#appflowy-code").fill("123");
  await page.waitForTimeout(5000);
  await expect(page.locator("#appflowy-code")).toHaveValue("123");
  await page.locator("#appflowy-code").fill("123456");
  await page.getByRole("button",{name:"Verify code"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Signed in as user@example.com");
});

test("workspace choice survives polling while signed in",async({page})=>{
  await page.evaluate(()=>{const f=(window as any).__af;f.workspaces=[{workspace_id:"ws1",workspace_name:"One"},{workspace_id:"ws2",workspace_name:"Two"}];f.appflowy={...f.appflowy,connected:true,email:"user@example.com"}});
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Signed in as user@example.com",{timeout:5000});
  await expect(page.locator('[name="appflowy_workspace_id"] option')).toHaveCount(2,{timeout:5000});
  await page.locator('[name="appflowy_workspace_id"]').selectOption("ws2");
  await page.waitForTimeout(5000);
  await expect(page.locator('[name="appflowy_workspace_id"]')).toHaveValue("ws2");
});

test("no unsaved Settings control is reset by background polling",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="appflowy_enabled"]').check();
  const edited=await page.evaluate(()=>{
    const out:Record<string,string|boolean>={};
    const page=document.querySelector('[data-view="settings"]')||document;
    for(const el of Array.from(page.querySelectorAll<HTMLInputElement|HTMLSelectElement>("input, select"))){
      const key=el.name||el.id; if(!key||el.disabled||(el as HTMLElement).offsetParent===null)continue;
      if(el instanceof HTMLInputElement&&el.type==="checkbox"){el.checked=!el.checked;el.dispatchEvent(new Event("change",{bubbles:true}));out[key]=el.checked}
      else if(el instanceof HTMLInputElement&&["text","email","url","search"].includes(el.type)&&!el.readOnly){el.value=el.value+"x";el.dispatchEvent(new Event("input",{bubbles:true}));out[key]=el.value}
      else if(el instanceof HTMLInputElement&&el.type==="number"){el.value=String(Number(el.value||0)+1);el.dispatchEvent(new Event("input",{bubbles:true}));out[key]=el.value}
    }
    return out;
  });
  expect(Object.keys(edited).length).toBeGreaterThan(5);
  await page.waitForTimeout(6000);
  const after=await page.evaluate((keys:string[])=>Object.fromEntries(keys.map(k=>{const el=document.querySelector<HTMLInputElement>(`[name="${k}"]`)||document.getElementById(k) as HTMLInputElement|null;return[k,el?(el.type==="checkbox"?el.checked:el.value):null]})),Object.keys(edited));
  expect(after).toEqual(edited);
});

test("a transcribed meeting offers AppFlowy setup when export is off, opening Settings",async({page})=>{
  const setup=page.getByRole("button",{name:"Set up AppFlowy export"});
  await expect(setup).toBeVisible();
  await setup.click();
  await expect(page.locator("#appflowy-settings-section")).toBeVisible();
  await expect(page.locator('[name="appflowy_enabled"]')).toBeVisible();
});

test("enabled but signed out offers sign-in instead of a dead export button",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:false}});
  await expect(page.getByRole("button",{name:"Sign in to AppFlowy"})).toBeVisible({timeout:5000});
  await expect(page.getByRole("button",{name:"Export to AppFlowy",exact:true})).toHaveCount(0);
});

test("full path: enable, sign in, then export from the meeting card",async({page})=>{
  await page.getByRole("button",{name:"Set up AppFlowy export"}).click();
  await page.locator('[name="appflowy_enabled"]').check();
  await page.getByRole("button",{name:"Save preferences"}).click();
  await page.evaluate(()=>{const f=(window as any).__af;f.appflowy={...f.appflowy,enabled:true}});
  await page.locator("#appflowy-email").fill("user@example.com");
  await page.getByRole("button",{name:"Send sign-in code"}).click();
  await page.locator("#appflowy-code").fill("123456");
  await page.getByRole("button",{name:"Verify code"}).click();
  await expect(page.locator("#appflowy-auth-status")).toContainText("Signed in as user@example.com");
  await page.getByRole("button",{name:"Dashboard"}).click();
  const exportButton=page.getByRole("button",{name:"Export to AppFlowy",exact:true});
  await expect(exportButton).toBeVisible({timeout:5000});
  await exportButton.click();
  await expect(page.getByRole("button",{name:"Export to AppFlowy again"})).toBeVisible({timeout:5000});
});

test("destination picker survives polling while signed in",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,connected:true,email:"user@example.com"}});
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator('[name="appflowy_parent_view_id"] option')).toHaveCount(3,{timeout:5000});
  await page.locator('[name="appflowy_parent_view_id"]').selectOption("page-1");
  await expect(page.locator("#appflowy-destination-path")).toContainText("Roadmap");
  await page.waitForTimeout(5000);
  await expect(page.locator('[name="appflowy_parent_view_id"]')).toHaveValue("page-1");
});

test("a completed export offers an Open in AppFlowy button that opens the created page",async({page})=>{
  await page.evaluate(()=>{(window as any).__af.appflowy={...(window as any).__af.appflowy,enabled:true,connected:true,email:"user@example.com",jobs:[{recording_id:"a",status:"completed",error:null,export_index:1,page_url:"https://appflowy.com/app/ws1/v1"}]}});
  await page.waitForTimeout(2100);
  await page.getByRole("button",{name:"Open in AppFlowy"}).click();
  expect(await page.evaluate(()=> (window as any).__af.calls.find((x:any)=>x[0]==="open_appflowy_url"))).toEqual(["open_appflowy_url",{url:"https://appflowy.com/app/ws1/v1"}]);
  expect(await page.evaluate(()=> (window as any).__af.openedUrls)).toEqual(["https://appflowy.com/app/ws1/v1"]);
});

test("previously created pages list renders escaped names with working open links",async({page})=>{
  await page.evaluate(()=>{
    const f=(window as any).__af;
    f.appflowy={...f.appflowy,connected:true,email:"user@example.com"};
    f.pages=[{key:"root:ws1:space1",kind:"root",name:"<img src=x onerror=alert(1)>",status:"created",url:"https://appflowy.com/app/ws1/root1",legacy:false}];
  });
  await page.getByRole("button",{name:"Settings"}).click();
  await expect(page.locator("#appflowy-previous-pages")).toContainText("<img src=x onerror=alert(1)>");
  await expect(page.locator("#appflowy-previous-pages img")).toHaveCount(0);
  await page.locator("#appflowy-previous-pages").locator("summary").click();
  await page.locator("#appflowy-previous-pages").getByRole("button",{name:"Open"}).click();
  expect(await page.evaluate(()=> (window as any).__af.openedUrls)).toEqual(["https://appflowy.com/app/ws1/root1"]);
});

test("a legacy previously-created page is labeled as possibly broken",async({page})=>{
  await page.evaluate(()=>{
    const f=(window as any).__af;
    f.appflowy={...f.appflowy,connected:true,email:"user@example.com"};
    f.pages=[
      {key:"root:ws1:space1",kind:"root",name:"My daily meetings",status:"created",url:"https://appflowy.com/app/ws1/root1",legacy:true},
      {key:"date:ws1:root1:2026-09-26",kind:"date",name:"2026-09-26",status:"created",url:"https://appflowy.com/app/ws1/date1",legacy:false},
    ];
  });
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#appflowy-previous-pages").locator("summary").click();
  const items=page.locator("#appflowy-previous-pages li");
  await expect(items.nth(0)).toContainText("created by an earlier version");
  await expect(items.nth(1)).not.toContainText("created by an earlier version");
});

test("meetings without a finished transcript show no AppFlowy action",async({page})=>{
  await page.evaluate(()=>{const f=(window as any).__af;f.appflowy={...f.appflowy,enabled:true,connected:true,email:"u@e.com"}});
  await page.route("**/*",r=>r.continue());
  await page.evaluate(()=>{const t=(window as any).__TAURI_INTERNALS__;const orig=t.invoke;t.invoke=async(c:string,a:any)=>c==="get_transcription_state"?{inhibited:false,model:{key:"base",available:false},jobs:[{recording_id:"a",status:"transcribing",progress:40,error:null,markdown_path:null}],error:null,backend:"cpu (AVX2)"}:orig(c,a)});
  await page.waitForTimeout(2500);
  await expect(page.locator("[data-appflowy-export],[data-appflowy-setup]")).toHaveCount(0);
});
