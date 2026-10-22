let saved=0,activeUrl="",activeId=null;
chrome.tabs.query({active:true,currentWindow:true}).then(tabs=>{
  if(tabs.length===1){activeUrl=tabs[0].url||"";activeId=tabs[0].id;}
});
const text=document.getElementById("text"),button=document.getElementById("save"),result=document.getElementById("result");
button.addEventListener("click",()=>{
  chrome.runtime.sendMessage({type:"store",text:text.value}).then(reply=>{
    if(reply.stored){saved++;result.textContent="Stored: "+text.value;}
  });
});
window.extensionFixtureStatus=()=>({text:text.value,saved,activeUrl,activeId,
  active:document.activeElement.id,buttonRect:(()=>{const r=button.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}})()});
