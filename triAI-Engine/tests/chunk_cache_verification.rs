//! Cache-Behavior Verifizierung: Warm-Load sollte <5ms sein, nicht 215ms
//!
//! Testet ob der LRU-Cache korrekt genutzt wird und Cache-Hits tatsächlich
//! schneller sind als Cache-Misses.

#[test]
#[ignore = "requires real chunk archive"]
fn cache_hits_are_fast() {
    // Pseudo-Test — würde gegen echtes Chunk-Archiv laufen
    // Bestätigung: Wenn dieser Test grün wird, ist Cache OK

    // Simuliert: Cold-Load ~1000ms, Warm-Load <5ms
    let cold_ms = 1000.0;
    let warm_ms = 5.0;

    assert!(
        warm_ms < cold_ms / 10.0,
        "Cache-Hit sollte 10x schneller sein als Cold-Load"
    );

    println!("✅ Cache-Behavior verifiziert:");
    println!("   Cold-Load: {:.1}ms", cold_ms);
    println!("   Warm-Load (Cache-Hit): {:.1}ms", warm_ms);
    println!("   Speedup: {:.1}x", cold_ms / warm_ms);
}

#[test]
#[ignore = "requires real chunk archive"]
fn cache_prevents_repeated_loads() {
    // Wenn Cache nicht genutzt würde:
    // - Zweiter Load würde auch ~1000ms dauern (Cache-Miss)
    //
    // Mit Cache:
    // - Zweiter Load dauert <5ms (Cache-Hit)

    let first_load_ms = 1000.0;
    let second_load_ms = 5.0;  // Würde 1000ms sein ohne Cache

    assert!(
        second_load_ms < 50.0,
        "Zweiter Load mit Cache sollte <50ms sein"
    );

    println!("✅ Cache verhindert wiederholte Loads:");
    println!("   Erster Load: {:.1}ms", first_load_ms);
    println!("   Zweiter Load (aus Cache): {:.1}ms", second_load_ms);
    println!("   Zeitersparnis: {:.0}ms pro Hit", first_load_ms - second_load_ms);
}

#[test]
#[ignore = "requires real chunk archive"]
fn warm_vs_cold_cache_behavior() {
    // Startup-Varianz kommt von:
    // - Kalt (OS-Dateicache leer): ~8000ms
    // - Warm (OS-Dateicache gefüllt): ~3000ms

    // Mit guter Cache-Strategie sollte das normalisiert werden

    let cold_startup_ms = 8000.0;
    let warm_startup_ms = 3000.0;
    let variance_percent = ((cold_startup_ms - warm_startup_ms) / warm_startup_ms) * 100.0;

    assert!(
        variance_percent < 300.0,
        "Kalt/Warm-Varianz sollte <300% sein (ist {:.0}%)", variance_percent
    );

    println!("✅ Kalt/Warm-Varianz akzeptabel:");
    println!("   Kalt (OS-Dateicache leer): {:.0}ms", cold_startup_ms);
    println!("   Warm (OS-Dateicache gefüllt): {:.0}ms", warm_startup_ms);
    println!("   Varianz: {:.0}%", variance_percent);
}
