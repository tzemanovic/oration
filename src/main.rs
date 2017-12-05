//! Oration: a Rocket/Elm self hosted commenting system for static sites.
//!
//! Inspired by ![Isso](https://posativ.org/isso/), which is a welcomed change from Disqus.
//! However, the codebase is unmaintained and ![security concerns](https://axiomatic.neophilus.net/posts/2017-04-16-from-disqus-to-isso.html) abound.
//! Oration aims to be a fast, lightweight and secure platform for your comments. Nothing more, but importantly, nothing less.


#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]
#![cfg_attr(feature="clippy", warn(missing_docs_in_private_items))]
#![cfg_attr(feature="clippy", warn(single_match_else))]

#![feature(plugin, custom_derive, use_extern_macros)]
#![plugin(rocket_codegen)]

// `error_chain!` can recurse deeply
#![recursion_limit = "1024"]

extern crate chrono;
extern crate dotenv;
#[macro_use]
extern crate error_chain;
extern crate rand;
extern crate rocket;
extern crate rocket_contrib;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate diesel;
extern crate r2d2_diesel;
extern crate r2d2;
extern crate yansi;
extern crate petgraph;
//extern crate argon2rs;
extern crate lettre;
extern crate lettre_email;
extern crate crypto;
extern crate reqwest;
extern crate serde_yaml;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
extern crate regex;
#[macro_use(log)]
extern crate log;

/// Loads configuration data from disk.
mod config;
/// Handles the database connection pool.
mod db;
/// SQL <----> Rust inerop using Diesel.
mod models;
/// Verbose schema for the comment database.
mod schema;
/// Serves up static files through Rocket.
mod static_files;
/// Handles the error chain of the program.
mod errors;
/// Tests for the Rocket side of the app.
#[cfg(test)]
mod tests;
/// Sends notification emails to admin.
mod notify;
/// Houses Data Structures that are needed in multiple modules.
mod data;

use std::io;
use rocket::response::NamedFile;
use std::net::SocketAddr;
use std::io::Cursor;
use rocket::http::Status;
use rocket::{State, Response};
use rocket::request::Form;
use rocket_contrib::Json;
use models::preferences::Preference;
use models::comments::{NestedComment, Comment};
use models::threads;
use std::process;
use yansi::Paint;
use config::Config;
use crypto::digest::Digest;
use crypto::sha2::Sha224;
use data::FormInput;

/// Serve up the index file. This is only useful for development. Should not be used in a release.
//TODO: Serve this some other way, we don't want oration doing this work.
#[get("/")]
fn index() -> io::Result<NamedFile> {
    NamedFile::open("public/index.html")
}

/// Process comment input from form.
#[post("/oration", data = "<comment>")]
fn new_comment<'a>(
    conn: db::Conn,
    comment: Result<Form<FormInput>, Option<String>>,
    config: State<Config>,
    remote_addr: SocketAddr,
) -> Response<'a> {
    let mut response = Response::new();
    match comment {
        Ok(f) => {
            //If the comment form data is valid, proceed to comment insertion
            let form = f.into_inner();
            let ip_addr = remote_addr.ip().to_string();
            //Get thread id from the db, create if needed
            match threads::gen_or_get_id(&conn, &config.host, &form.title, &form.path) {
                Ok(tid) => {
                    if let Err(err) = Comment::insert(
                        &conn,
                        tid,
                        &form,
                        &ip_addr,
                        config.nesting_limit,
                    )
                    {
                        //Something went wrong, return a 500
                        log::warn!("{}", &err);
                        for e in err.iter().skip(1) {
                            log::warn!("    {} {}", Paint::white("=> Caused by:"), Paint::red(&e));
                        }
                        response.set_status(Status::InternalServerError);
                    } else {
                        //All good, 200
                        response.set_status(Status::Ok);
                        response.set_sized_body(Cursor::new("Comment recieved."));
                        //Send notification to admin
                        if config.notifications.new_comment {
                            match notify::send_notification(
                                &form,
                                &config.notifications,
                                &config.host,
                                &config.blog_name,
                                &ip_addr,
                            ) {
                                Ok(_) => {
                                    log::info!(
                                        "📧  {}",
                                        Paint::blue("New comment email notification sent.")
                                    )
                                }
                                Err(err) => {
                                    log::warn!("{}", &err);
                                    for e in err.iter().skip(1) {
                                        log::warn!(
                                            "    {} {}",
                                            Paint::white("=> Caused by:"),
                                            Paint::red(&e)
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    //We didn't get the thread id
                    match err {
                        errors::Error(errors::ErrorKind::PathCheckFailed, _) => {
                            //The requsted path doesn't exist on the server
                            //Most likely an attempt at injecting junk into the db through the post method
                            response.set_status(Status::Forbidden)
                        }
                        _ => response.set_status(Status::InternalServerError),
                    }
                }
            }
        }
        Err(Some(f)) => {
            //The form request was malformed, 400
            response.set_status(Status::BadRequest);
            response.set_sized_body(Cursor::new(format!("Invalid form input: {}", f)));
        }
        Err(None) => {
            //Not UTF-8 encoded
            response.set_status(Status::BadRequest);
            response.set_sized_body(Cursor::new("Form input was invalid UTF8."));
        }
    }
    response
}

/// Information sent to the client upon initialisation.
#[derive(Serialize)]
struct Initialise {
    /// The clients' ip address, hashed via Sha224.
    user_ip: String,
    /// The Sha224 hash of the blog author to distinguish the authority on this blog.
    blog_author: String,
}

/// Gets a Sha224 hash from a clients IP along with the blog's author hash.
#[get("/oration/init")]
fn initialise(remote_addr: SocketAddr, config: State<Config>) -> Json<Initialise> {

    let ip_addr = remote_addr.ip().to_string();
    // create a Sha224 object
    let mut hasher = Sha224::new();
    // write input message
    hasher.input_str(&ip_addr);

    let to_send = Initialise {
        user_ip: hasher.result_str(),
        blog_author: config.author.hash.to_owned(),
    };

    Json(to_send)
}

/// Test function that returns the session hash from the database.
#[get("/oration/session")]
fn get_session(conn: db::Conn) -> String {
    match Preference::get_session(&conn) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("{}", err);
            for e in err.iter().skip(1) {
                log::warn!("    {} {}", Paint::white("=> Caused by:"), Paint::red(&e));
            }
            err.to_string()
        }
    }
}

#[derive(FromForm)]
/// Used in conjuction with `/count?` and `/comments?`.
struct Post {
    /// Gets the url for the request.
    url: String,
}

#[derive(Serialize)]
/// Comments to frontend
struct PostComments {
    /// A nested set of comments.
    comments: Vec<NestedComment>,
}

/// Return a json block of comment data for the requested url.
#[get("/oration/comments?<post>")]
fn get_comments(conn: db::Conn, post: Post) -> Option<Json<PostComments>> {
    //TODO: The logic here may not 100%, need to consider / vs /index.* for example.
    match NestedComment::list(&conn, &post.url) {
        Ok(comments) => {
            //We now have a vector of comments
            let to_send = PostComments { comments: comments };
            Some(Json(to_send))
        }
        Err(err) => {
            log::warn!("{}", err);
            for e in err.iter().skip(1) {
                log::warn!("    {} {}", Paint::white("=> Caused by:"), Paint::red(&e));
            }
            None
        }
    }
}

/// Returns the comment count for a given post from the database.
#[get("/oration/count?<post>")]
fn get_comment_count(conn: db::Conn, post: Post) -> String {
    //TODO: The logic here may not 100%, need to consider / vs /index.* for example.
    match Comment::count(&conn, &post.url) {
        Ok(s) => s.to_string(),
        Err(err) => {
            log::warn!("{}", err);
            for e in err.iter().skip(1) {
                log::warn!("    {} {}", Paint::white("=> Caused by:"), Paint::red(&e));
            }
            err.to_string()
        }
    }
}

/// Ignite Rocket, connect to the database and start serving data.
/// Exposes a connection to the database so we can set the session on startup.
fn rocket() -> (rocket::Rocket, db::Conn, String) {
    //Load configuration data from disk
    let config = match Config::load() {
        Ok(c) => c,
        Err(ref err) => {
            println!("Error loading configuration: {}", err);
            for e in err.iter().skip(1) {
                println!("caused by: {}", e);
            }
            process::exit(1)
        }
    };
    let host = config.host.clone();
    let pool = db::init_pool();
    let conn = match pool.get() {
        Ok(p) => db::Conn(p),
        Err(err) => {
            println!("Could not connect to database: {}", err);
            process::exit(1)
        }
    };
    let rocket = rocket::ignite().manage(pool).manage(config).mount(
        "/",
        routes![
            index, //TODO: index and static_files should not be managed by oration
            static_files::files,
            new_comment,
            initialise,
            get_session,
            get_comment_count,
            get_comments,
        ],
    );

    (rocket, conn, host)
}

/// Application entry point.
fn main() {
    //Initialise webserver routes and database connection pool
    let (rocket, conn, host) = rocket();

    //Set the session info in the database
    log::info!("💿  {}", Paint::purple("Saving session hash to database"));
    match Preference::set_session(&conn) {
        Ok(set) => {
            if !set {
                //TODO: This may need to be a crit as well. Unsure.
                log::warn!("Failed to set session hash");
            }
        }
        Err(err) => {
            log::error!("{}", err);
            for e in err.iter().skip(1) {
                log::error!("    {} {}", Paint::white("=> Caused by:"), Paint::red(&e));
            }
            process::exit(1);
        }
    };

    log::info!(
        "📢  {} {}",
        Paint::blue("Oration will serve comments to"),
        host
    );

    //Start the web service
    rocket.launch();
}
