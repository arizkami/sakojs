console.log("before throw");
function inner() { throw new Error("boom"); }
inner();
