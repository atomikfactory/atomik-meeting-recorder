import {expect,test} from "@playwright/test";
test.beforeEach(async({page})=>{await page.addInitScript(()=>{
  const rec=(id:string)=>({recording_id:id,started_at:1,ended_at:2,duration_ms:1,status:"recorded",tracks:[],error:null,transcription_status:"",appflowy_status:""});
  const f:any={
    calls:[],
    settings:{auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",diarization_enabled:true,audio_sources:[],summary_enabled:true,ollama_url:"http://127.0.0.1:11434",summary_model:"qwen3:8b",appflowy_enabled:false,appflowy_auto_export:false,appflowy_base_url:"https://beta.appflowy.cloud",appflowy_use_native_block:true,appflowy_workspace_id:null},
    appflowy:{enabled:false,auto_export:false,connected:false,email:null,base_url:"https://beta.appflowy.cloud",jobs:[],inhibited:false,error:null},
    workspaces:[{workspace_id:"ws1",workspace_name:"Test Workspace"}],
    otpOk:true,
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
    if(c==="appflowy_request_otp"){if(!f.otpOk)throw Error("AppFlowy Cloud is not reachable right now.");f.otpEmail=a.email;return null}
    if(c==="appflowy_verify_otp"){if(a.code!=="123456")throw Error("invalid code");f.appflowy={...f.appflowy,connected:true,email:a.email};return null}
    if(c==="appflowy_sign_out"){f.appflowy={...f.appflowy,connected:false,email:null};return null}
    if(c==="appflowy_export_now"){f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:1}]};return null}
    if(c==="appflowy_export_again"){if(!a.confirm)throw Error("Exporting again requires confirmation.");f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"completed",error:null,export_index:2}]};return null}
    if(c==="appflowy_retry"){f.appflowy={...f.appflowy,jobs:[{recording_id:a.recordingId,status:"queued",error:null,export_index:1}]};return null}
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
