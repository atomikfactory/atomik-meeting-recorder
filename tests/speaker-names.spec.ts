import {expect,test} from "@playwright/test";

test.beforeEach(async({page})=>{await page.addInitScript(()=>{
  const rec=(id:string)=>({recording_id:id,started_at:1,ended_at:2,duration_ms:1,status:"recorded",tracks:[
    {source_id:"mic",name:"Microphone",direction:"input",sample_rate:16000,channels:1,file_path:"mic.wav",frames_written:1,dropped_samples:0,error:null},
    {source_id:"loop",name:"Meeting audio (loopback)",direction:"output",sample_rate:16000,channels:1,file_path:"loop.wav",frames_written:1,dropped_samples:0,error:null},
  ],error:null,transcription_status:"",appflowy_status:""});
  const f:any={
    calls:[],
    settings:{auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",diarization_enabled:true,audio_sources:[],recognize_known_voices:true},
    labels:{a:[{label:"You",name:null,auto_identified:false},{label:"Speaker 1",name:null,auto_identified:false},{label:"Remote",name:null,auto_identified:false}]},
    changedSince:{},
    profiles:[] as any[],
    rememberShouldFail:false,
    reidentifyOutcomes:[] as any[],
  };
  Object.assign(window,{__sn:f,__TAURI_INTERNALS__:{transformCallback:()=>1,invoke:async(c:string,a:any)=>{
    f.calls.push([c,a]);
    if(c==="plugin:event|listen")return 1;
    if(c==="get_settings")return f.settings;
    if(c==="save_settings"){f.settings={...f.settings,...a.settings};return f.settings}
    if(c==="get_app_state")return{status:"idle",recording_supported:true,detection_supported:false,appflowy_connected:false,recordings_folder:"x",tray_available:true,tray_error:null};
    if(c==="get_recording_state")return{status:"idle",active:null,recent:[rec("a")],error:null};
    if(c==="get_detection_state")return{supported:false,enabled:false,phase:"unsupported",detail:"",provider:null,error:null,seconds_remaining:null,automation_inhibited:false};
    if(c==="get_transcription_state")return{inhibited:false,model:{key:"base",available:false},jobs:[{recording_id:"a",status:"completed",progress:100,error:null,markdown_path:null}],error:null,backend:"cpu (AVX2)"};
    if(c==="get_diarization_state")return{enabled:true,inhibited:false,models:[],jobs:[{recording_id:"a",status:"completed",progress:100,error:null,speaker_count:1}],error:null};
    if(c==="get_summary_state")return{enabled:true,inhibited:false,jobs:[{recording_id:"a",status:"completed",progress:100,error:null,title:null}],error:null};
    if(c==="get_compression_state")return{enabled:true,inhibited:false,jobs:[],error:null};
    if(c==="audio_storage_summary")return{total_bytes:0,total_original_bytes:0,recordings:[]};
    if(c==="get_audio_devices")return{backend:"x",devices:[],errors:[]};
    if(c==="list_speaker_labels")return f.labels[a.recordingId]??[];
    if(c==="set_speaker_name"){
      const list=f.labels[a.recordingId]??(f.labels[a.recordingId]=[]);
      const row=list.find((l:any)=>l.label===a.label);
      if(row){row.name=a.name;row.auto_identified=false}
      if(a.remember&&/^Speaker \d+$/.test(a.label)&&f.rememberShouldFail)throw Error("Not enough speech to remember this voice (2.0s of at least 5s needed).");
      return null;
    }
    if(c==="delete_speaker_name"){
      const list=f.labels[a.recordingId]??[];
      const row=list.find((l:any)=>l.label===a.label);
      if(row){row.name=null;row.auto_identified=false}
      return null;
    }
    if(c==="speaker_names_changed_since_summary")return f.changedSince[a.recordingId]??false;
    if(c==="regenerate_summary"){f.changedSince[a.recordingId]=false;return null}
    if(c==="reidentify_speakers")return f.reidentifyOutcomes;
    if(c==="list_voice_profiles")return f.profiles;
    if(c==="rename_voice_profile"){const p=f.profiles.find((x:any)=>x.id===a.id);if(p)p.name=a.name;return null}
    if(c==="delete_voice_profile"){f.profiles=f.profiles.filter((x:any)=>x.id!==a.id);return null}
    if(c==="forget_all_voice_profiles"){f.profiles=[];return null}
    throw Error(c)
  }}})
});await page.goto("/")});

test("speakers panel renders every label",async({page})=>{
  await page.getByRole("button",{name:"Speakers"}).click();
  await expect(page.locator('[data-speaker-row="You"]')).toContainText("You (set in Settings)");
  await expect(page.locator('[data-speaker-row="Speaker 1"]')).toBeVisible();
  await expect(page.locator('[data-speaker-row="Remote"]')).toBeVisible();
});

test("typing a name survives several poll cycles",async({page})=>{
  await page.getByRole("button",{name:"Speakers"}).click();
  const input=page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]');
  await input.fill("Alice");
  await page.waitForTimeout(4000);
  await expect(input).toHaveValue("Alice");
});

test("save calls set_speaker_name with recording id, label, name and remember",async({page})=>{
  await page.getByRole("button",{name:"Speakers"}).click();
  await page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]').fill("Alice");
  await page.locator('[data-speaker-save="a"][data-speaker-label="Speaker 1"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="set_speaker_name"))).toEqual(["set_speaker_name",{recordingId:"a",label:"Speaker 1",name:"Alice",remember:true}]);
});

test("unchecking remember saves remember:false",async({page})=>{
  await page.getByRole("button",{name:"Speakers"}).click();
  await page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]').fill("Alice");
  await page.locator('[data-speaker-remember="a"][data-speaker-label="Speaker 1"]').uncheck();
  await page.locator('[data-speaker-save="a"][data-speaker-label="Speaker 1"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="set_speaker_name")?.[1].remember)).toBe(false);
});

test("a backend refusal to remember a voice is shown verbatim",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.rememberShouldFail=true});
  await page.getByRole("button",{name:"Speakers"}).click();
  await page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]').fill("Alice");
  await page.locator('[data-speaker-save="a"][data-speaker-label="Speaker 1"]').click();
  await expect(page.locator('[data-speaker-panel-error="a"]')).toContainText("Not enough speech to remember this voice");
  // The name itself still saved despite the remember failure.
  await expect(page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]')).toHaveValue("Alice");
});

test("an auto-identified name shows an auto marker and can be corrected",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.labels.a=[{label:"You",name:null,auto_identified:false},{label:"Speaker 1",name:"Alice",auto_identified:true},{label:"Remote",name:null,auto_identified:false}]});
  await page.getByRole("button",{name:"Speakers"}).click();
  await expect(page.locator('[data-speaker-row="Speaker 1"] [data-speaker-auto-badge]')).toBeVisible();
  await page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]').fill("Bob");
  await page.locator('[data-speaker-save="a"][data-speaker-label="Speaker 1"]').click();
  await expect(page.locator('[data-speaker-row="Speaker 1"] [data-speaker-auto-badge]')).toHaveCount(0);
  await expect(page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]')).toHaveValue("Bob");
});

test("clear name calls delete_speaker_name",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.labels.a=[{label:"You",name:null,auto_identified:false},{label:"Speaker 1",name:"Alice",auto_identified:false},{label:"Remote",name:null,auto_identified:false}]});
  await page.getByRole("button",{name:"Speakers"}).click();
  await page.locator('[data-speaker-clear="a"][data-speaker-label="Speaker 1"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="delete_speaker_name"))).toEqual(["delete_speaker_name",{recordingId:"a",label:"Speaker 1"}]);
});

test("re-identify speakers calls reidentify_speakers",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.reidentifyOutcomes=[{speaker:1,name:"Alice",score:0.9}]});
  await page.getByRole("button",{name:"Speakers"}).click();
  await page.locator('[data-reidentify-speakers="a"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="reidentify_speakers"))).toEqual(["reidentify_speakers",{recordingId:"a"}]);
});

test("the regenerate-summary hint appears and its button regenerates",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.changedSince.a=true});
  await page.getByRole("button",{name:"Speakers"}).click();
  await expect(page.locator("text=Speaker names changed")).toBeVisible();
  await page.locator('[data-speaker-regenerate-summary="a"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="regenerate_summary"))).toEqual(["regenerate_summary",{recordingId:"a"}]);
  await expect(page.locator("text=Speaker names changed")).toHaveCount(0);
});

test("a speaker name is rendered as escaped text, never executed",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.labels.a=[{label:"You",name:null,auto_identified:false},{label:"Speaker 1",name:"<img src=x onerror=alert(1)>",auto_identified:false},{label:"Remote",name:null,auto_identified:false}]});
  await page.getByRole("button",{name:"Speakers"}).click();
  await expect(page.locator('[data-speaker-name-input][data-speaker-label="Speaker 1"]')).toHaveValue("<img src=x onerror=alert(1)>");
  await expect(page.locator("img[src=x]")).toHaveCount(0);
});

test("Settings lists voice profiles and a rename draft survives polling",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.profiles=[{id:"p1",name:"Alice",sample_seconds:42,updated_at:1700000000}]});
  await page.getByRole("button",{name:"Settings"}).click();
  const nameInput=page.locator('[data-voice-profile-name="p1"]');
  await expect(nameInput).toHaveValue("Alice");
  await nameInput.fill("Alicia");
  await page.waitForTimeout(3000);
  await expect(nameInput).toHaveValue("Alicia");
  await page.locator('[data-voice-profile-rename="p1"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="rename_voice_profile"))).toEqual(["rename_voice_profile",{id:"p1",name:"Alicia"}]);
});

test("deleting a voice profile requires confirmation",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.profiles=[{id:"p1",name:"Alice",sample_seconds:42,updated_at:1700000000}];window.confirm=()=>false});
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[data-voice-profile-delete="p1"]').click();
  expect(await page.evaluate(()=>(window as any).__sn.calls.some((x:any)=>x[0]==="delete_voice_profile"))).toBeFalsy();
});

test("confirming delete removes the voice profile",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.profiles=[{id:"p1",name:"Alice",sample_seconds:42,updated_at:1700000000}];window.confirm=()=>true});
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[data-voice-profile-delete="p1"]').click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.some((x:any)=>x[0]==="delete_voice_profile"))).toBeTruthy();
});

test("forget all voices requires confirmation then calls the command",async({page})=>{
  await page.evaluate(()=>{(window as any).__sn.profiles=[{id:"p1",name:"Alice",sample_seconds:42,updated_at:1700000000}];window.confirm=()=>true});
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator("#forget-all-voices").click();
  await expect.poll(()=>page.evaluate(()=>(window as any).__sn.calls.some((x:any)=>x[0]==="forget_all_voice_profiles"))).toBeTruthy();
});

test("toggling recognize known voices saves the setting",async({page})=>{
  await page.getByRole("button",{name:"Settings"}).click();
  await page.locator('[name="recognize_known_voices"]').uncheck();
  await page.getByRole("button",{name:"Save preferences"}).click();
  expect(await page.evaluate(()=>(window as any).__sn.calls.find((x:any)=>x[0]==="save_settings")[1].settings.recognize_known_voices)).toBe(false);
});
