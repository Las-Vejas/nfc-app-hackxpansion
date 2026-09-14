#![no_std]

// We use Box and other types that require the heap, so we have to include this
extern crate alloc;

use alloc::boxed::Box;
use core::{future::Future, pin::Pin};

use xpanse_api::{
    app::App,
    interfaces::nfc::Nfc,
    reexports::{
        defmt,
        embassy_time::Timer,
        slint::{self, ComponentHandle},
    },
    registry::{Registry, ResourceLease},
};

// This will include all the components that we define in `.slint` files
slint::include_modules!();

// Every app is a struct that has all the resources that it needs inside it
pub struct ButtonLoggerApp {
    button: ResourceLease<Box<dyn Button<A>>>,
}

// Every app needs to implement the App trait
impl App for ButtonLoggerApp {
    // This will be the display name in the app list
    const NAME: &'static str = "Button Logger";

    // This function checks if the registry has all
    // the resources that this app *needs* to run, like controls/buttons.
    //
    // And if it can run, it will be added to the app list
    fn can_run(registry: &Registry) -> bool {
        // We check if the registry has an A button
        registry.has::<Box<dyn Button<A>>>()
    }

    // This function takes all the resources that the app needs
    // and creates a new app instance if everything is successful
    //
    // This function can try to take resources that the app
    // doesn't *need* to run. For example, a game can run without sound,
    // but not without buttons.
    fn new(registry: &mut Registry) -> Option<Self> {
        // We try to take an A button
        let button = registry.take_resource::<Box<dyn Button<A>>>()?;
        Some(Self { button })
    }

    // This is where the actual app runs
    fn run<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        // We need a Box because of Rust's async trait requirements
        Box::pin(async move {
            // We try to create an instance of our exported component
            let ui = match ButtonLoggerUI::new() {
                Ok(ui) => ui,
                Err(_) => {
                    defmt::error!("ButtonLoggerApp: failed to create UI");
                    return;
                }
            };

            // We try to show this component on the screen
            if ui.show().is_err() {
                defmt::error!("ButtonLoggerApp: failed to show UI");
                return;
            }

            let mut count = 0u32;
            loop {
                // We wait until the button is pressed
                self.button.resource_mut().wait_for_pressed().await;

                // We count how many milliseconds the button was held
                let mut held_ms = 0;
                while self.button.resource().is_pressed() && held_ms < 1_000 {
                    Timer::after_millis(20).await;
                    held_ms += 20;
                }

                // If the button was held for more than 1000 ms, we exit from the app
                if held_ms >= 1_000 {
                    while self.button.resource().is_pressed() {
                        Timer::after_millis(20).await;
                    }
                    break;
                }

                // If the press was shorter than 1,000 ms, we add 1 to `count`
                count += 1;
                // Slint automatically generates `set_` functions for reactive variables
                ui.set_count(count as i32);
                // Log the count to the console
                defmt::info!("button A pressed (count: {})", count);
            }

            // Hide the UI when the app exits
            if ui.hide().is_err() {
                defmt::error!("ButtonLoggerApp: failed to hide UI");
            }
        })
    }

    // Since we took a button from the registry,
    // we should also give it back
    fn release(self, registry: &mut Registry) {
        registry.return_resource(self.button);
    }
}