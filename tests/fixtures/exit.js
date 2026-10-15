for (let i = 0; i < 3; i++) console.log("line", i);
setTimeout(() => console.log("never"), 1000);
process.exit(7);
