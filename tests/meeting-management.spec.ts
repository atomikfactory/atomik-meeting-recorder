import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const track = { source_id:"mic", name:"Desk mic", direction:"input", sample_rate:48000, channels:1, file_path:"retained.wav", frames_written:480, dropped_samples:0, error:null };
    const recording = (id:string, status="recorded", title="Weekly <planning>") => ({ recording_id:id, started_at:1, ended_at:2, duration_ms:1000, status, tracks:[track], error:status === "interrupted" ? "device-reported discontinuities" : null, meeting_title:title, storage_path:`meetings/example--${id}`, transcription_status:"", appflowy_status:"" });
    const fixture:any = { calls:[] as any[], recent:[recording("a", "interrupted"), recording("b", "failed", "Other")], jobs:[{recording_id:"a",status:"completed",progress:100,error:null,markdown_path:"local.md",model_key:"base"}], busy:false, resolveRestart:null };
    Object.assign(window, { __meetingManagement:fixture, __TAURI_INTERNALS__:{ transformCallback:()=>1, invoke:async(command:string,args:any) => {
      fixture.calls.push([command,args]);
      if(command === "plugin:event|listen") return 1;
      if(command === "get_settings") return {auto_record:true,start_debounce_seconds:10,end_grace_seconds:30,whisper_model:"base",audio_sources:[]};
      if(command === "get_app_state") return {status:"idle",recording_supported:true,detection_supported:false,appflowy_connected:false,recordings_folder:"test",tray_available:true,tray_error:null};
      if(command === "get_audio_devices") return {backend:"test",errors:[],devices:[]};
      if(command === "get_recording_state") return {status:"idle",active:null,recent:fixture.recent,error:null,levels:[]};
      if(command === "get_detection_state") return {supported:false,enabled:false,phase:"unsupported",provider:null,detail:"",error:null,seconds_remaining:null,automation_inhibited:false};
      if(command === "get_transcription_state") return {inhibited:false,model:{key:"base",available:true},jobs:fixture.jobs,error:null,backend:"cpu (AVX2)"};
      if(command === "get_diarization_state") return {enabled:false,inhibited:false,models:[],jobs:[],error:null};
      if(command === "rename_recording") { fixture.recent.find((r:any)=>r.recording_id===args.recordingId).meeting_title=args.title; return null; }
      if(command === "delete_recording") { fixture.recent=fixture.recent.filter((r:any)=>r.recording_id!==args.recordingId); return null; }
      if(command === "restart_transcription") { fixture.busy=true; return new Promise(resolve=>{fixture.resolveRestart=()=>{fixture.busy=false;resolve(null)}}); }
      if(command === "transcribe_recording") return null;
      throw Error(`Unexpected ${command}`);
    }}});
  });
  await page.goto("/");
});

const calls = (page:any, command:string) => page.evaluate(([command]) => (window as any).__meetingManagement.calls.filter((x:any)=>x[0]===command), [command]);

test("audio quality warning remains distinct from completed local transcript", async ({page}) => {
  const row=page.locator("#recent-recordings");
  await expect(row).toContainText("discontinuities");
  await expect(row).toContainText("Transcript complete");
});

test("rename cancel makes no IPC call; confirmation preserves escaped title after poll", async ({page}) => {
  await page.getByRole("button",{name:"Rename"}).first().click();
  await expect(page.locator("#management-dialog")).toBeVisible();
  await expect(page.locator("#management-name")).toHaveValue("Weekly <planning>");
  await page.locator("#management-cancel").click();
  await expect(calls(page,"rename_recording")).resolves.toEqual([]);
  await page.getByRole("button",{name:"Rename"}).first().click();
  await page.locator("#management-name").fill("<img src=x>");
  // A background poll must not discard an open management operation or its draft.
  await page.waitForTimeout(1100);
  await expect(page.locator("#management-name")).toHaveValue("<img src=x>");
  await page.locator("#management-confirm").click();
  await expect.poll(()=>calls(page,"rename_recording")).toEqual([["rename_recording",{recordingId:"a",title:"<img src=x>"}]]);
  await page.waitForTimeout(1100);
  await expect(page.locator("#recent-recordings")).toContainText("<img src=x>");
  await expect(page.locator("img[src=x]")).toHaveCount(0);
});

test("remove cancellation is inert and confirmation hides only its target while retaining files wording", async ({page}) => {
  await page.getByRole("button",{name:"Remove"}).first().click();
  await expect(page.locator("#management-detail")).toContainText("recoverable");
  await page.locator("#management-cancel").click();
  await expect(calls(page,"delete_recording")).resolves.toEqual([]);
  await page.getByRole("button",{name:"Remove"}).first().click();
  await page.locator("#management-confirm").click();
  await expect.poll(()=>calls(page,"delete_recording")).toEqual([["delete_recording",{recordingId:"a"}]]);
  await expect(page.locator("#recent-recordings")).not.toContainText("Weekly <planning>");
  await expect(page.locator("#recent-recordings")).toContainText("Other");
});

test("restart transcription confirms exact recording and model and prevents duplicate mutation", async ({page}) => {
  await page.getByRole("button",{name:"Restart transcription"}).click();
  await expect(page.locator("#management-detail")).toContainText("base");
  await expect(page.locator("#management-detail")).toContainText(/archiv/i);
  await page.locator("#management-confirm").click();
  await expect(page.locator("#management-confirm")).toBeDisabled();
  await expect(page.locator("#management-cancel")).toBeDisabled();
  await page.locator("#management-dialog").press("Escape");
  await expect(page.locator("#management-dialog")).toBeVisible();
  await expect.poll(()=>calls(page,"restart_transcription")).toEqual([["restart_transcription",{recordingId:"a"}]]);
  await page.waitForTimeout(1100);
  await expect.poll(()=>calls(page,"restart_transcription")).toEqual([["restart_transcription",{recordingId:"a"}]]);
  await page.evaluate(()=> (window as any).__meetingManagement.resolveRestart());
});

test("failed retained capture offers Transcribe", async ({page}) => {
  await expect(page.locator("#recent-recordings").getByRole("button",{name:"Transcribe"})).toBeVisible();
});
